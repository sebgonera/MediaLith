//! What time this appliance thinks it is, in the one format the trust chain compares.
//!
//! Certificate expiry is a string comparison against RFC 3339 in UTC (ADR-0006), so
//! something has to turn a `SystemTime` into that shape. Written here rather than taken
//! from a crate for the reason the rest of this image is written the way it is: a date
//! library is a dependency, a build, and a supply-chain surface, for thirty lines of
//! arithmetic that has been settled since 1582.
//!
//! # The appliance cannot really tell the time
//!
//! There is an RTC and no time synchronisation. A clock that is wrong and *believed*
//! refuses every future update, which from outside is indistinguishable from a bricked
//! update path — and unlike an expired certificate, nobody would know where to look. So
//! this module also answers "is the clock plausible", by comparing it against the one
//! timestamp the image is certain of: the build stamp it carries in its own version
//! string. An image cannot predate itself.
//!
//! # What has run
//!
//! **Nothing on hardware.** The conversion is pinned against `date -u` output in its
//! tests; no appliance has yet judged a certificate with it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds in a day.
const DAY: u64 = 86_400;

/// The device's idea of the current time, RFC 3339 in UTC.
///
/// `None` if the clock is before the epoch, which a machine can produce with a dead RTC
/// and which has no honest rendering here.
#[must_use]
pub fn now() -> Option<String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| rfc3339(since.as_secs()))
}

/// Renders seconds since the epoch as RFC 3339 in UTC.
///
/// Days are converted with Howard Hinnant's civil-from-days algorithm, which is exact for
/// the proleptic Gregorian calendar and needs no table. Leap seconds do not exist in Unix
/// time, so there is nothing here that drifts.
#[must_use]
pub fn rfc3339(unix_seconds: u64) -> String {
    let days = unix_seconds / DAY;
    let seconds = unix_seconds % DAY;

    // Shifted so the era starts on 0000-03-01, which puts the leap day at the end of a
    // year and removes every special case from what follows.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    let year = if month <= 2 { year + 1 } else { year };

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
    )
}

/// The UTC build stamp out of a version string: `0.1.0.202607281844` → `202607281844`.
///
/// MediaLith versions are a release followed by a `YYYYMMDDHHMM` stamp. That stamp is the
/// only timestamp on the appliance that cannot be wrong, because the build wrote it rather
/// than reading it from a clock — which is what makes it both the sanity check for the
/// clock ([`built_at`]) and the anti-rollback counter (`crate::sequence`).
///
/// One parser for both, deliberately. Two readers of the same field are two chances to
/// disagree about which release a machine is running, and they would disagree in the place
/// where the answer decides whether an update is a downgrade.
///
/// `None` for a version with no stamp, or one whose stamp is not a plausible date.
#[must_use]
pub fn build_stamp(release: &str) -> Option<u64> {
    let stamp = release.rsplit('.').next()?;
    if stamp.len() != 12 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let stamp: u64 = stamp.parse().ok()?;

    // Range-checked rather than merely numeric. A stamp of 209913451844 would otherwise
    // become a date far in the future, and a future build date is the direction that
    // refuses every certificate rather than the direction that accepts too much.
    let (year, month, day) = (
        stamp / 100_000_000,
        (stamp / 1_000_000) % 100,
        (stamp / 10_000) % 100,
    );
    let (hour, minute) = ((stamp / 100) % 100, stamp % 100);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || year < 2020
    {
        return None;
    }

    Some(stamp)
}

/// The instant an image was built, RFC 3339, out of the version string it carries.
///
/// `None` when [`build_stamp`] finds no usable stamp. A caller that gets `None` knows only
/// that it cannot judge its own clock, which is the truth and is handled by not checking
/// expiry — never by inventing a build date the appliance would then believe.
#[must_use]
pub fn built_at(release: &str) -> Option<String> {
    let stamp = build_stamp(release)?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:00Z",
        stamp / 100_000_000,
        (stamp / 1_000_000) % 100,
        (stamp / 10_000) % 100,
        (stamp / 100) % 100,
        stamp % 100,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_instants_the_way_date_does() {
        // Pinned against `date -u -d @N`, not against this function's own output. A
        // conversion tested only against itself is the failure shape this project has
        // already paid for in plexos-types::partition.
        for (seconds, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_769_904_000, "2026-02-01T00:00:00Z"),
            (1_785_000_000, "2026-07-25T17:20:00Z"),
            (4_102_444_800, "2100-01-01T00:00:00Z"),
        ] {
            assert_eq!(rfc3339(seconds), expected, "at {seconds}");
        }
    }

    #[test]
    fn a_rendered_instant_sorts_as_a_string_the_way_it_does_as_a_time() {
        // The whole reason the format matters: expiry is decided by comparing these as
        // strings, so zero padding is load-bearing rather than cosmetic.
        let earlier = rfc3339(1_769_904_000);
        let later = rfc3339(1_785_000_000);
        assert!(earlier < later, "{earlier} must sort below {later}");
        assert!(rfc3339(0) < earlier);
    }

    #[test]
    fn a_version_string_yields_the_moment_the_image_was_built() {
        assert_eq!(
            built_at("0.1.0.202607281844").as_deref(),
            Some("2026-07-28T18:44:00Z")
        );
    }

    #[test]
    fn the_build_stamp_agrees_with_the_clock_conversion() {
        // Two independent paths to the same instant: one parses the version string, the
        // other converts seconds. If they ever disagree, an appliance would judge its own
        // clock against a moment it never built at.
        assert_eq!(
            built_at("0.1.0.202607281844").unwrap(),
            rfc3339(1_785_264_240)
        );
    }

    #[test]
    fn the_stamp_and_the_instant_come_from_one_parser() {
        // Two readers of this field would be two chances to disagree about which release a
        // machine is running, in the place where that decides whether an update is a
        // downgrade.
        assert_eq!(build_stamp("0.1.0.202607281844"), Some(202_607_281_844));
        assert_eq!(build_stamp("0.1.0"), None);
        assert!(built_at("0.1.0.202607281844").is_some());
    }

    #[test]
    fn a_version_with_no_usable_stamp_says_so_rather_than_inventing_one() {
        // The consequence of None is that expiry goes unchecked, which is deliberate: an
        // invented build date is a clock this appliance would then believe.
        for release in [
            "0.1.0",
            "0.1.0.2026",
            "0.1.0.20260728184x",
            "0.1.0.209913451844",
            "0.1.0.201907281844",
            "",
        ] {
            assert_eq!(built_at(release), None, "{release} should not parse");
        }
    }

    #[test]
    fn the_clock_this_machine_has_renders_at_all() {
        // Not an assertion about what time it is -- there is no right answer to that on a
        // machine with no time synchronisation. Only that the path produces something of
        // the right shape, which is what the comparison depends on.
        let now = now().expect("a build host's clock is after 1970");
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z'), "{now}");
        assert!(now.as_str() > "2020-01-01T00:00:00Z", "{now}");
    }
}
