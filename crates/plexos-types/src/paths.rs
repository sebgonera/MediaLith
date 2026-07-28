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

/// Root of state owned by PlexOS itself.
pub const PLEXOS_STATE: &str = "/var/lib/plexos";

/// Layout version of `/var`, read by `plexos-init` before any service starts.
pub const STATE_VERSION_FILE: &str = "/var/lib/plexos/STATE_VERSION";

/// Upper layer of the `/etc` overlay: persistent configuration.
pub const PLEXOS_ETC: &str = "/var/lib/plexos/etc";

/// The declarative configuration file, as seen through the overlay.
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

/// Pre-migration snapshots of small state (ADR-0009).
pub const BACKUP: &str = "/var/lib/plexos/backup";

/// Plex Media Server's data directory.
///
/// Exported to Plex as `PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR`. Its internal
/// structure belongs to Plex: PlexOS backs it up and never edits it.
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
