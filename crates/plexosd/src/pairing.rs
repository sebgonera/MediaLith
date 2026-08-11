//! Pairing a browser from the machine's own screen (ADR-0019).
//!
//! The device token (ADR-0013) is a recovery credential: high entropy, stored as a
//! fingerprint, shown once, and typed by hand. It is the right shape for the thing it is
//! — the way back into an appliance nobody can reach — and the wrong shape for the thing
//! it had been doing as well, which is getting a browser administrating in the first
//! place. Sixteen characters transcribed from a panel is a minute of somebody's evening
//! and a 403 when they read `8` for `B`.
//!
//! This module is the other half: a code that exists for five minutes, works once, and is
//! never typed at all, because it travels in a QR code on the screen the person is already
//! standing in front of.
//!
//! # The physical action is the security boundary
//!
//! Nothing here can be started over the network. There is no route that generates a
//! pairing code, and that absence is the design rather than an omission — a `POST
//! /api/start-pairing` reachable from the LAN would let anyone on it make the appliance
//! produce a credential and then guess at nothing, because the appliance would have
//! offered a fresh one to whoever asked. [`Offers::offer`] is called from the dashboard's
//! keyboard handler and from the first-boot screen, and from nowhere else.
//!
//! That is the same trust model ADR-0013 already states in different words: whoever can
//! read the attached screen may administer the device. Pressing a key on that screen is a
//! stronger claim than reading it, not a weaker one.
//!
//! # Why the secret is held in plaintext here, and why that is not a lapse
//!
//! Everywhere else in this crate a credential is reduced to its SHA-256 and the plaintext
//! is discarded — `auth` does it, `session` does it. This does not, and the reason is that
//! the plaintext is **on a monitor** for as long as the offer stands. A fingerprint beside
//! it would protect nothing that the screen is not already showing to the room, while
//! adding a second representation to keep in step.
//!
//! What matters is the property that *is* real: **it never reaches a disk.** There is no
//! file, no `/run` state and no log line carrying it, so a pulled disk yields nothing and
//! a reboot ends every offer that was outstanding.
//!
//! # Expiry is monotonic
//!
//! [`Instant`], not the wall clock. This appliance has no RTC synchronisation of any kind
//! (`docs/PRODUCTION-READINESS.md` lists it as unowned), so the day something sets the
//! clock is the day a wall-clock deadline moves. A correction backwards would turn a
//! five-minute code into one good until the correction is undone, which is the one
//! direction that must not be possible.
//!
//! # What has run
//!
//! **Nothing on hardware at the time of writing.** The tests below drive [`Offers`]
//! directly with a synthetic timeline, which is the only way to exercise a five-minute
//! deadline without waiting five minutes.

use std::io::Read as _;
use std::time::{Duration, Instant};

/// Bytes of entropy in a pairing code.
///
/// 16, which is 128 bits, and unlike the device token this number was not traded against
/// legibility: nobody types this. It is read by a camera, so the only cost of entropy is
/// QR modules, and 128 bits costs about four characters more than 96 would.
pub const SECRET_BYTES: usize = 16;

/// Characters in a pairing code: [`SECRET_BYTES`] at five bits each, rounded up.
///
/// 26 for 128 bits. The rounding is up rather than down because the encoder emits a final
/// character for the leftover bits rather than dropping them, and a constant that
/// disagreed with the encoder would be a length check that fails on every real code.
pub const SECRET_CHARS: usize = (SECRET_BYTES * 8).div_ceil(5);

/// How long an offer stands.
///
/// Five minutes. Long enough to find a phone, unlock it and open the camera; short enough
/// that a code photographed by somebody who then walked away is worthless by the time they
/// are anywhere.
pub const LIFETIME: Duration = Duration::from_secs(5 * 60);

/// Why a presented pairing code was not accepted.
///
/// Three cases and not one, because the remedies differ and a browser that says only "no"
/// sends somebody back to a machine they have already walked away from. None of them
/// tells the presenter anything about the code that *is* on offer — in particular there
/// is nothing here that gets closer with a better guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing is pairing. Either nothing ever was, or an offer was used or cancelled.
    NotOffered,
    /// There was an offer and its five minutes are up.
    Expired,
    /// An offer stands and this was not it.
    Wrong,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOffered => write!(
                f,
                "this pairing code is no longer valid. Remedy: press P on the screen \
                 attached to the appliance to show a new one."
            ),
            Self::Expired => write!(
                f,
                "this pairing code has expired. Remedy: press P on the screen attached to \
                 the appliance to show a new one."
            ),
            Self::Wrong => write!(
                f,
                "this is not the pairing code the appliance is showing. Remedy: scan the \
                 QR code currently on its screen, or press P for a new one."
            ),
        }
    }
}

/// A code on offer, and when it stops being one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Offer {
    secret: String,
    expires_at: Instant,
}

/// The one pairing code the appliance is offering, if it is offering one.
///
/// A struct rather than a bare global so the whole of its behaviour can be tested on an
/// instance with a synthetic clock. Rust runs tests as threads in one process, and a suite
/// built on a global would be one test cancelling what another was about to consume — a
/// shape this repository has already been bitten by twice.
#[derive(Debug, Default)]
pub struct Offers {
    current: Option<Offer>,
}

impl Offers {
    /// Nothing on offer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts `secret` on offer for [`LIFETIME`], replacing anything already there.
    ///
    /// Replacing rather than refusing is what makes "press P again" the remedy for every
    /// state this can be in. The previous code stops working at that moment, which is the
    /// property somebody pressing P a second time is relying on: they are pressing it
    /// because they no longer trust the first one, or because they lost track of it.
    pub fn offer(&mut self, secret: String, now: Instant) {
        self.current = Some(Offer {
            secret,
            expires_at: now + LIFETIME,
        });
    }

    /// Withdraws the offer. Idempotent, so ESC on a screen showing nothing is harmless.
    pub fn cancel(&mut self) {
        self.current = None;
    }

    /// The code on offer, for drawing it. `None` once it has expired.
    ///
    /// Expiry is checked here rather than only in [`Self::consume`] so that a screen
    /// cannot keep showing a QR code that would be refused. A visibly-valid code that the
    /// server rejects is the worst state this feature has available to it: the machine
    /// contradicts itself and the person believes the machine.
    #[must_use]
    pub fn secret(&self, now: Instant) -> Option<&str> {
        self.current
            .as_ref()
            .filter(|offer| now < offer.expires_at)
            .map(|offer| offer.secret.as_str())
    }

    /// How long the current offer has left, for the countdown.
    ///
    /// `None` when nothing is on offer. `Some(ZERO)` never happens: an offer with nothing
    /// left is an expired one, and expired is reported as absent so that one state has one
    /// representation.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.current
            .as_ref()
            .and_then(|offer| offer.expires_at.checked_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    /// Whether an offer was made and has since run out.
    ///
    /// Separate from [`Self::remaining`] because the screen says different things: an
    /// appliance that never offered anything shows its dashboard, and one whose code has
    /// just expired says so and names the key that makes another.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        self.current
            .as_ref()
            .is_some_and(|offer| now >= offer.expires_at)
    }

    /// Spends the offer if `presented` is it.
    ///
    /// Single use is enforced by taking the offer out of `self` before returning success,
    /// under whatever lock the caller holds. There is no window in which two callers can
    /// both succeed, because the check and the removal are one `&mut self` call rather
    /// than a read followed by a write.
    ///
    /// A wrong guess costs the offer nothing. That is deliberate and follows ADR-0013's
    /// reasoning about lockouts: against 128 bits there is nothing to guess, so a rule
    /// that invalidated the code on a wrong attempt would give anyone who can reach the
    /// port a way to stop the owner pairing, and buy nothing in exchange.
    ///
    /// # Errors
    /// [`Refusal`], which distinguishes "nothing on offer", "expired" and "not it".
    pub fn consume(&mut self, presented: &str, now: Instant) -> Result<(), Refusal> {
        let Some(offer) = self.current.as_ref() else {
            return Err(Refusal::NotOffered);
        };
        if now >= offer.expires_at {
            // Cleared, so the next attempt reports NotOffered rather than Expired for
            // ever. The screen has already stopped showing it.
            self.current = None;
            return Err(Refusal::Expired);
        }
        if !crate::auth::constant_time_eq(presented, &offer.secret) {
            return Err(Refusal::Wrong);
        }
        self.current = None;
        Ok(())
    }
}

/// Reads a fresh pairing code from the kernel.
///
/// Crockford base32 through [`crate::auth::encode_token`], which is not for legibility
/// here — nobody reads this — but for the QR code: upper-case letters and digits are the
/// alphanumeric mode of ISO/IEC 18004, which packs 5.5 bits per character against byte
/// mode's 8. A lower-case or base64 code would make a visibly denser symbol for the same
/// entropy, and density is the whole of whether a phone can read it off a monitor.
///
/// # Errors
/// Only if `/dev/urandom` cannot be read, which means `/dev` is not mounted.
pub fn generate() -> std::io::Result<String> {
    let mut bytes = [0_u8; SECRET_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(crate::auth::encode_token(&bytes))
}

/// The appliance's one set of offers.
static CURRENT: std::sync::Mutex<Offers> = std::sync::Mutex::new(Offers { current: None });

fn offers() -> std::sync::MutexGuard<'static, Offers> {
    CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Generates a code, puts it on offer, and returns it for drawing.
///
/// # Errors
/// If randomness cannot be read.
pub fn start() -> std::io::Result<String> {
    let secret = generate()?;
    offers().offer(secret.clone(), Instant::now());
    Ok(secret)
}

/// Withdraws whatever is on offer.
pub fn cancel() {
    offers().cancel();
}

/// The code currently on offer, or `None`.
#[must_use]
pub fn secret() -> Option<String> {
    offers().secret(Instant::now()).map(ToOwned::to_owned)
}

/// How long the current offer has left.
#[must_use]
pub fn remaining() -> Option<Duration> {
    offers().remaining(Instant::now())
}

/// Whether an offer was made and has run out.
#[must_use]
pub fn is_expired() -> bool {
    offers().is_expired(Instant::now())
}

/// Spends the offer if `presented` is it.
///
/// # Errors
/// [`Refusal`].
pub fn consume(presented: &str) -> Result<(), Refusal> {
    offers().consume(presented, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A timeline that starts now and goes forwards.
    ///
    /// Forwards only, and built from one base: `Instant` is monotonic and has no
    /// arithmetic that is guaranteed to work backwards from an arbitrary point, so a test
    /// that said "thirteen hours ago" would be a test that panics on a machine which
    /// booted twenty minutes ago.
    fn timeline() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_generated_code_carries_the_entropy_the_adr_claims() {
        let secret = generate().expect("/dev/urandom");
        assert_eq!(
            secret.len(),
            SECRET_CHARS,
            "128 bits at five bits per character"
        );
        assert!(
            secret
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "the QR's alphanumeric mode takes upper case and digits only: {secret}"
        );
    }

    #[test]
    fn two_generated_codes_differ() {
        // The whole class of mistake where the "random" source is a constant, a zeroed
        // buffer, or a read nobody checked.
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a, b);
        assert_ne!(a, "0".repeat(SECRET_CHARS), "not a zeroed buffer");
    }

    #[test]
    fn the_right_code_is_accepted() {
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("ABC123".to_owned(), t0);
        assert_eq!(
            offers.consume("ABC123", t0 + Duration::from_secs(1)),
            Ok(())
        );
    }

    #[test]
    fn a_wrong_code_is_refused_and_leaves_the_offer_standing() {
        // No lockout, for ADR-0013's reason: against 128 bits there is nothing to guess,
        // so invalidating on a wrong attempt would hand anyone who can reach the port a
        // way to stop the owner pairing and buy nothing.
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("RIGHT".to_owned(), t0);

        for _ in 0..50 {
            assert_eq!(offers.consume("WRONG", t0), Err(Refusal::Wrong));
        }
        assert_eq!(offers.consume("RIGHT", t0), Ok(()), "still the live code");
    }

    #[test]
    fn a_code_works_exactly_once() {
        // The property the whole module exists for. A code that could be replayed is a
        // credential lying on a monitor for anyone who photographed it.
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("ONCE".to_owned(), t0);

        assert_eq!(offers.consume("ONCE", t0), Ok(()));
        assert_eq!(
            offers.consume("ONCE", t0),
            Err(Refusal::NotOffered),
            "the second browser gets nothing"
        );
    }

    #[test]
    fn a_code_expires_after_five_minutes() {
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("TIMED".to_owned(), t0);

        // Built by adding a shorter duration rather than by subtracting from the deadline.
        // clippy forbids an unchecked `Duration` subtraction and is right to: the same
        // expression against a shorter lifetime would underflow and panic in the test
        // rather than failing its assertion.
        let just_before = t0 + Duration::from_secs(LIFETIME.as_secs() - 1);
        assert!(offers.consume("TIMED", just_before).is_ok());

        offers.offer("TIMED".to_owned(), t0);
        assert_eq!(
            offers.consume("TIMED", t0 + LIFETIME),
            Err(Refusal::Expired),
            "the deadline is inclusive: at exactly five minutes it is over"
        );
    }

    #[test]
    fn an_expired_code_stops_being_drawn_before_anybody_presents_it() {
        // A QR on screen that the server would refuse is the worst state available here:
        // the machine contradicts itself, and the person believes the machine.
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("TIMED".to_owned(), t0);

        assert_eq!(offers.secret(t0), Some("TIMED"));
        assert!(offers.remaining(t0).is_some());

        assert_eq!(offers.secret(t0 + LIFETIME), None, "nothing left to draw");
        assert_eq!(offers.remaining(t0 + LIFETIME), None);
        assert!(
            offers.is_expired(t0 + LIFETIME),
            "and the screen can say why"
        );
    }

    #[test]
    fn generating_another_code_invalidates_the_first() {
        // Somebody presses P a second time because they no longer trust the first code or
        // have lost track of it. Both readings require the first one to stop working.
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("FIRST".to_owned(), t0);
        offers.offer("SECOND".to_owned(), t0 + Duration::from_secs(10));

        assert_eq!(
            offers.consume("FIRST", t0 + Duration::from_secs(11)),
            Err(Refusal::Wrong)
        );
        assert!(
            offers
                .consume("SECOND", t0 + Duration::from_secs(11))
                .is_ok()
        );
    }

    #[test]
    fn the_second_codes_five_minutes_start_when_it_was_made() {
        // Not inherited from the first. A rolling deadline would make a code that has been
        // on a screen for an hour still valid because somebody kept pressing P.
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("FIRST".to_owned(), t0);

        let later = t0 + Duration::from_secs(4 * 60);
        offers.offer("SECOND".to_owned(), later);
        assert_eq!(
            offers.remaining(later),
            Some(LIFETIME),
            "a fresh five minutes, not the minute the first one had left"
        );
    }

    #[test]
    fn cancelling_invalidates_the_code_at_once() {
        // ESC on the screen. Somebody who decides not to pair after all has to be able to
        // take the offer back, or the remedy for "I showed that to the wrong person" is
        // waiting five minutes.
        let t0 = timeline();
        let mut offers = Offers::new();
        offers.offer("SHOWN".to_owned(), t0);
        offers.cancel();

        assert_eq!(offers.consume("SHOWN", t0), Err(Refusal::NotOffered));
        assert_eq!(offers.secret(t0), None);
        assert!(!offers.is_expired(t0), "cancelled is not expired");
    }

    #[test]
    fn cancelling_nothing_is_harmless() {
        let mut offers = Offers::new();
        offers.cancel();
        assert_eq!(
            offers.consume("ANYTHING", timeline()),
            Err(Refusal::NotOffered)
        );
    }

    #[test]
    fn a_machine_that_has_just_started_is_offering_nothing() {
        // What a reboot leaves behind, and the reason this state is the default rather
        // than something that has to be arrived at: the offer lives in one process's
        // memory, so a restarted daemon has no offers and no sessions.
        let offers = Offers::new();
        let t0 = timeline();
        assert_eq!(offers.secret(t0), None);
        assert_eq!(offers.remaining(t0), None);
        assert!(!offers.is_expired(t0));
    }

    #[test]
    fn nothing_here_writes_the_secret_anywhere() {
        // The property that replaces "only the fingerprint is stored": there is no path
        // out of this module to a disk. Asserted against the source, because the absence
        // of a write is not something a unit test can observe.
        //
        // The module *above* the tests, and that is not fussiness. The first version read
        // the whole file and failed on itself, because the list of things that must not
        // appear is written here in the test that forbids them. It is the trap the build
        // script already carries -- `card button {` passing a presence check because grep
        // found it inside the assertion about its absence -- arriving from the other
        // direction: grep the artefact, never the prose written about it.
        let whole = include_str!("pairing.rs");
        let source = whole
            .split_once("#[cfg(test)]")
            .expect("this module has tests")
            .0;
        for forbidden in ["fs::write", "File::create", "OpenOptions", "/var/", "/run/"] {
            assert!(
                !source.contains(forbidden),
                "a pairing secret must never reach a disk, and {forbidden} is how it would"
            );
        }
    }

    #[test]
    fn every_refusal_names_a_remedy() {
        // The rule plexos-gpu enforces with a test of its own. "No" on a phone, with the
        // appliance in another room, has reproduced the problem it was reporting.
        for refusal in [Refusal::NotOffered, Refusal::Expired, Refusal::Wrong] {
            let said = refusal.to_string();
            assert!(said.contains("Remedy:"), "{refusal:?}: {said}");
            assert!(
                said.contains('P') || said.contains("QR"),
                "and the remedy has to name the physical action: {said}"
            );
        }
    }

    #[test]
    fn the_lifetime_is_the_five_minutes_the_adr_states() {
        assert_eq!(LIFETIME, Duration::from_secs(300));
        assert_eq!(SECRET_BYTES * 8, 128, "the entropy ADR-0019 claims");
    }
}
