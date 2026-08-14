//! Canonical filesystem paths (ADR-0009).
//!
//! Defined once so that `plexos-init`, `plexosd`, the updater, and the installer cannot
//! disagree about where persistent state lives. A path that appears in only one of them
//! is a path that will be forgotten by a migration or a backup.
//!
//! The dividing line: **everything outside `/var` is discarded on reboot.** `/` is a
//! tmpfs, `/usr` is a read-only verified image, and `/etc` is an overlay whose upper
//! layer is [`PLEXOS_ETC`]. Code writing anywhere else and expecting the result to
//! survive is a bug, and it will be caught on the next reboot rather than months later.

/// Read-only verified system image.
pub const USR: &str = "/usr";

/// Factory `/etc` content shipped in the image; the overlay's lower layer.
pub const ETC_FACTORY: &str = "/usr/share/factory/etc";

/// The only persistent partition.
pub const VAR: &str = "/var";

/// Root of state owned by MediaLith itself.
///
/// # Legacy internal path, retained deliberately
///
/// The product is MediaLith; this path still says `plexos`, and every path below it does
/// too. That is the point rather than an oversight.
///
/// `/var` is the one surface an update does not replace and a **rollback does not
/// revert** — which is exactly why the device token, the TLS key, the anti-rollback floor
/// and the revocation list live here. ADR-0005's whole mechanism assumes a release can
/// fail its health gate and hand the machine back to the one before it — including one
/// published before the rename, which must still find its own state where it left it.
///
/// Renaming this is therefore not a rename but a state migration, and it would have to
/// leave the *previous* release able to read the result. Public branding does not need to
/// match an on-disk namespace, and here it deliberately does not.
pub const PLEXOS_STATE: &str = "/var/lib/plexos";

/// Layout version of `/var`, read by `plexos-init` before any service starts.
pub const STATE_VERSION_FILE: &str = "/var/lib/plexos/STATE_VERSION";

/// Upper layer of the `/etc` overlay: persistent configuration.
pub const PLEXOS_ETC: &str = "/var/lib/plexos/etc";

/// The declarative configuration file, as seen through the overlay.
///
/// Legacy internal path retained for rollback compatibility after the MediaLith rename,
/// and this one is sharper than it looks: `/etc` is an overlay whose upper layer is
/// [`PLEXOS_ETC`] on `/var`, so this file is *persistent state wearing a `/etc` address*.
/// Renaming it would leave an existing machine's hostname, timezone and static addressing
/// sitting in a file nothing reads any more — the settings would revert to defaults with
/// nothing reporting that they had.
pub const CONFIG_FILE: &str = "/etc/plexos/config.toml";

/// Plex app images, named by upstream version (ADR-0007).
pub const PLEX_APPS: &str = "/var/lib/plexos/apps/plex";

/// Symlink to the Plex app image currently in use. Swapped atomically on update.
pub const PLEX_CURRENT: &str = "/var/lib/plexos/apps/plex/current";

/// Mount point of the active Plex app image.
pub const PLEX_MOUNT: &str = "/run/plexos/plex";

/// Update staging: partial downloads, accepted sequence, revocation list.
pub const UPDATE_STATE: &str = "/var/lib/plexos/update";

/// Highest manifest sequence ever accepted; the anti-rollback floor (ADR-0006).
pub const ACCEPTED_SEQUENCE_FILE: &str = "/var/lib/plexos/update/accepted_sequence";

/// Root-signed signing-key revocation list.
pub const REVOCATION_FILE: &str = "/var/lib/plexos/update/revocations.json";

/// Why the last boot was handed back to the other slot.
///
/// On `/var` for the reason that makes `/var` awkward everywhere else: rollback reverts
/// `/usr` and never this (ADR-0005, ADR-0009). Everything describing a failed boot lives
/// in the image that failed, so it goes away exactly when it becomes interesting — the
/// system that comes back is the *older* one, and it has no way to know it is a
/// replacement rather than a machine that simply restarted. This file is the only place
/// a note can outlive the thing it is about.
pub const ROLLBACK_RECORD_FILE: &str = "/var/lib/plexos/update/rollback.json";

/// The console's TLS identity: its key, its certificate, and what it was issued for.
///
/// On `/var` because the appliance issues its own certificate and the key must survive a
/// reboot — and, more sharply, must survive an *update*. The fingerprint a person checked
/// once is the fingerprint of this key; regenerating it on every boot would train them to
/// ignore the one warning that matters.
pub const TLS_DIR: &str = "/var/lib/plexos/tls";

/// Pre-migration snapshots of small state (ADR-0009).
pub const BACKUP: &str = "/var/lib/plexos/backup";

/// Plex Media Server's data directory.
///
/// Exported to Plex as `PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR`. Its internal
/// structure belongs to Plex: MediaLith backs it up and never edits it.
pub const PLEX_DATA: &str = "/var/lib/plex";

/// Transcoding scratch space. Safe to delete at any time.
pub const PLEX_TRANSCODE_DIR: &str = "/var/cache/plex-transcode";

/// Default mount point for library storage.
pub const MEDIA: &str = "/var/media";

/// The unprivileged user Plex Media Server runs as (ADR-0007).
///
/// **Frozen, and more thoroughly than a path.** This number owns every file Plex
/// writes under [`PLEX_DATA`] and [`PLEX_TRANSCODE_DIR`], both of which survive an OS
/// update and an OS rollback. Changing it does not rename anything: it orphans a media
/// database that the new Plex cannot read and the old one no longer owns, on a
/// filesystem that ADR-0009 says a migration may only add to.
///
/// Below 1000 because it is a system account with no login, and 900 rather than a
/// lower number to stay clear of the ranges Buildroot's own packages allocate from.
pub const PLEX_UID: u32 = 900;

/// The group Plex runs as. Same reasoning as [`PLEX_UID`], same freeze.
pub const PLEX_GID: u32 = 900;

/// Its name in `/etc/passwd`, which the Buildroot users table must agree with.
pub const PLEX_USER: &str = "plex";

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path that must survive a reboot has to be under `/var`. This test is the
    /// guard against a future path constant being added in the wrong place.
    #[test]
    fn persistent_paths_live_under_var() {
        let persistent = [
            PLEXOS_STATE,
            STATE_VERSION_FILE,
            PLEXOS_ETC,
            PLEX_APPS,
            PLEX_CURRENT,
            UPDATE_STATE,
            ACCEPTED_SEQUENCE_FILE,
            REVOCATION_FILE,
            TLS_DIR,
            BACKUP,
            PLEX_DATA,
            MEDIA,
        ];
        for path in persistent {
            assert!(
                path.starts_with("/var/"),
                "{path} must be under /var to survive a reboot"
            );
        }
    }

    #[test]
    fn ephemeral_paths_are_not_under_var() {
        for path in [PLEX_MOUNT, USR, ETC_FACTORY, CONFIG_FILE] {
            assert!(!path.starts_with("/var/"), "{path} is not persistent state");
        }
    }

    #[test]
    fn transcode_scratch_is_deletable_cache_not_state() {
        assert!(
            PLEX_TRANSCODE_DIR.starts_with("/var/cache/"),
            "transcode scratch must be under /var/cache so it is never backed up"
        );
    }

    #[test]
    fn the_plex_account_is_a_system_account_and_stays_put() {
        // This uid owns files on /var, which outlives the OS image and survives a
        // rollback. Changing it orphans a media database rather than renaming anything,
        // so it is pinned here the way a partition GUID is: the test exists to make the
        // change deliberate, not to check arithmetic.
        assert_eq!(PLEX_UID, 900);
        assert_eq!(PLEX_GID, 900);
        assert_eq!(PLEX_USER, "plex");
        let uid = PLEX_UID;
        assert!(uid < 1000, "a system account, with no login");
        assert_ne!(uid, 0, "the entire point is that it is not root");
    }

    #[test]
    fn the_buildroot_users_table_creates_the_account_this_crate_names() {
        // The two are edited in different files and only Buildroot reads one of them,
        // so a mismatch surfaced as a failed image build forty minutes in -- or worse,
        // as a Plex handed a data directory it does not own.
        //
        // It has already gone wrong once: the table said `-900`, which mkusers reads as
        // "allocate one for me" rather than as the number 900, and rejects outright
        // below -2. An allocated uid differs between builds, which is exactly the
        // orphaned-database problem this constant exists to prevent.
        let table = include_str!("../../../buildroot/board/plexos/x86_64/users.table");
        let entry = table
            .lines()
            .find(|line| line.starts_with(&format!("{PLEX_USER} ")))
            .expect("a plex entry in the users table");

        let fields: Vec<&str> = entry.split_whitespace().collect();
        assert_eq!(fields[1], PLEX_UID.to_string(), "uid: {entry}");
        assert_eq!(fields[3], PLEX_GID.to_string(), "gid: {entry}");
        assert!(
            !fields[1].starts_with('-') && !fields[3].starts_with('-'),
            "a negative id means 'allocate one', which is not what this pins: {entry}"
        );
    }

    #[test]
    fn state_paths_are_nested_under_the_state_root() {
        for path in [
            STATE_VERSION_FILE,
            PLEXOS_ETC,
            PLEX_APPS,
            UPDATE_STATE,
            BACKUP,
        ] {
            assert!(
                path.starts_with(PLEXOS_STATE),
                "{path} escapes the state root"
            );
        }
    }
}
