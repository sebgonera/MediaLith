//! Deciding what to do about the persistent state layout (ADR-0009).
//!
//! `/var` is the only thing on a PlexOS device that cannot be recreated, and it is the
//! one part of the system that **rollback does not revert**. The OS returns to the
//! previous `/usr` and kernel; the Plex library, the config, and the update state all
//! stay as the newer release left them.
//!
//! That asymmetry creates the trap this module exists to avoid. A release that migrates
//! `/var` into a shape its predecessor cannot read has built a system where rollback
//! works perfectly and then fails on state it no longer understands — turning the
//! safety mechanism into the thing that bricks the machine.
//!
//! So the rule, stated once and enforced by [`decide`]: **finding newer state than we
//! expect is never fatal.** Restore what the older release can read, log loudly, and
//! boot. A device that will not boot after a correct rollback is strictly worse than
//! one running a slightly older configuration.

use std::fmt;

/// What `plexos-init` must do before any service starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAction {
    /// The layout matches. Carry on.
    Proceed,
    /// A fresh `/var`. Create the layout at the current version.
    Initialise {
        /// Version to stamp onto the new state.
        to: u32,
    },
    /// Older state. Run migrations in sequence, each backed up first.
    Migrate {
        /// Version found on disk.
        from: u32,
        /// Version to reach.
        to: u32,
    },
    /// Newer state than this release understands: we have rolled back.
    ///
    /// Restore the pre-migration backup for anything unreadable here and boot anyway.
    RestoreForRollback {
        /// Version found on disk, written by a newer release.
        found: u32,
        /// Version this release implements.
        expected: u32,
    },
}

impl StateAction {
    /// Whether this action modifies `/var`.
    ///
    /// Used to decide whether a pre-flight backup is needed, and to keep a read-only
    /// recovery boot honest about what it will not do.
    #[must_use]
    pub const fn writes_state(self) -> bool {
        !matches!(self, Self::Proceed)
    }

    /// Whether the running release is older than the state it found.
    #[must_use]
    pub const fn is_rollback(self) -> bool {
        matches!(self, Self::RestoreForRollback { .. })
    }
}

impl fmt::Display for StateAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Proceed => f.write_str("state layout is current"),
            Self::Initialise { to } => {
                write!(f, "initialising a fresh /var at layout version {to}")
            }
            Self::Migrate { from, to } => {
                write!(f, "migrating /var from layout version {from} to {to}")
            }
            Self::RestoreForRollback { found, expected } => write!(
                f,
                "/var is at layout version {found} but this release implements \
                 {expected}; this is a rollback, restoring compatible state and \
                 continuing"
            ),
        }
    }
}

/// Decides what to do, given what is on disk and what this release implements.
///
/// A pure function, because this is the decision that must never be wrong and must be
/// exhaustively testable without a filesystem.
#[must_use]
pub const fn decide(found: Option<u32>, expected: u32) -> StateAction {
    match found {
        None => StateAction::Initialise { to: expected },
        Some(v) if v == expected => StateAction::Proceed,
        Some(v) if v < expected => StateAction::Migrate {
            from: v,
            to: expected,
        },
        Some(v) => StateAction::RestoreForRollback { found: v, expected },
    }
}

/// Parses the contents of `STATE_VERSION`.
///
/// An unreadable or nonsensical file is treated as absent rather than as an error.
/// The alternative is refusing to boot over a corrupted single-integer file, which
/// would be a spectacular way to lose a machine.
#[must_use]
pub fn parse_state_version(contents: &str) -> Option<u32> {
    contents.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_types::version::STATE_LAYOUT_VERSION;

    #[test]
    fn matching_versions_proceed_without_touching_var() {
        let action = decide(Some(3), 3);
        assert_eq!(action, StateAction::Proceed);
        assert!(!action.writes_state());
        assert!(!action.is_rollback());
    }

    #[test]
    fn a_fresh_var_is_initialised_at_the_current_version() {
        assert_eq!(decide(None, 4), StateAction::Initialise { to: 4 });
    }

    #[test]
    fn older_state_migrates_forward() {
        assert_eq!(decide(Some(1), 3), StateAction::Migrate { from: 1, to: 3 });
    }

    #[test]
    fn newer_state_is_a_rollback_and_must_still_boot() {
        // The rule this module exists for. Refusing to boot here would turn every
        // rollback taken after a migration into a brick.
        let action = decide(Some(5), 3);
        assert_eq!(
            action,
            StateAction::RestoreForRollback {
                found: 5,
                expected: 3
            }
        );
        assert!(action.is_rollback());
        assert!(
            action.writes_state(),
            "a rollback restores compatible state, so it does write"
        );
    }

    #[test]
    fn no_input_produces_a_refusal_to_boot() {
        // Exhaustive over a realistic range: every combination yields an action, and
        // none of them is "give up".
        for found in 0..8 {
            for expected in 0..8 {
                let action = decide(Some(found), expected);
                assert!(
                    matches!(
                        action,
                        StateAction::Proceed
                            | StateAction::Migrate { .. }
                            | StateAction::RestoreForRollback { .. }
                    ),
                    "found={found} expected={expected} produced {action:?}"
                );
            }
        }
    }

    #[test]
    fn a_corrupt_version_file_is_treated_as_a_fresh_var_not_an_error() {
        for bad in [
            "",
            "   ",
            "not a number",
            "3.1",
            "-1",
            "999999999999999999999",
        ] {
            assert_eq!(parse_state_version(bad), None, "should reject {bad:?}");
        }
        assert_eq!(
            decide(parse_state_version("garbage"), 2),
            StateAction::Initialise { to: 2 }
        );
    }

    #[test]
    fn parses_a_well_formed_version_file() {
        assert_eq!(parse_state_version("1\n"), Some(1));
        assert_eq!(parse_state_version("  42  \n"), Some(42));
    }

    #[test]
    fn messages_explain_themselves_to_someone_reading_a_boot_log() {
        let rollback = decide(Some(5), 3).to_string();
        assert!(rollback.contains("rollback"), "got: {rollback}");
        assert!(rollback.contains("continuing"), "got: {rollback}");

        assert!(decide(Some(1), 2).to_string().contains("migrating"));
    }

    #[test]
    fn the_current_release_proceeds_against_its_own_layout() {
        assert_eq!(
            decide(Some(STATE_LAYOUT_VERSION), STATE_LAYOUT_VERSION),
            StateAction::Proceed
        );
    }
}
