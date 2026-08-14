//! Parsing the kernel command line.
//!
//! MediaLith gets two things from the command line that it cannot get anywhere else: which
//! `/usr` slot to boot, and the dm-verity root hash to verify it against. Both live
//! inside the signed Unified Kernel Image, so the Secure Boot signature covers them
//! (ADR-0004) — which is precisely why they arrive this way rather than from a config
//! file on a writable partition.
//!
//! A command line this module cannot parse is fatal, and deliberately so. Without a
//! slot there is nothing to mount, and without a root hash there is no way to verify
//! what we would mount. Guessing either would defeat the entire trust chain, so the
//! only correct response is to fail the boot and let the rollback in ADR-0005 pick the
//! other slot.

use std::fmt;

use plexos_types::Slot;

/// Legacy internal namespace, retained after the MediaLith product rename.
///
/// These three keys still say `plexos.`, and renaming them would break the one thing this
/// appliance must never break. The command line lives *inside the signed UKI*, so each
/// release carries its own — including the previous release a rollback lands on, whose
/// UKI was built when these were the names. A MediaLith `plexos-init` that only understood
/// `medialith.slot` would boot its own image and refuse the one behind it, which is the
/// image ADR-0005 exists to fall back to.
///
/// The parser is what would have to accept both for a change here to be safe, and the
/// value of doing so is a string nobody reads unless something has already gone wrong.
const KEY_SLOT: &str = "plexos.slot";
/// Command line key carrying the dm-verity root hash. Legacy namespace; see [`KEY_SLOT`].
const KEY_ROOTHASH: &str = "plexos.roothash";
/// Command line key requesting a debug shell instead of normal startup. Legacy namespace.
const KEY_DEBUG_SHELL: &str = "plexos.debug_shell";

/// What the kernel told us about this boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootArgs {
    /// The `/usr` slot this UKI was built for.
    pub slot: Slot,
    /// dm-verity root hash of that slot's image, lowercase hex.
    pub root_hash: String,
    /// Whether to drop to a shell rather than starting services.
    ///
    /// Only honoured by development builds; see [`BootArgs::parse`].
    pub debug_shell: bool,
}

/// Why a command line could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdlineError {
    /// No `plexos.slot` parameter.
    MissingSlot,
    /// `plexos.slot` names something that is not a slot.
    UnknownSlot(String),
    /// No `plexos.roothash` parameter.
    MissingRootHash,
    /// The root hash is not lowercase hex of a plausible length.
    MalformedRootHash(String),
}

impl fmt::Display for CmdlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSlot => write!(
                f,
                "kernel command line has no {KEY_SLOT}; this UKI was not built by MediaLith \
                 image tooling"
            ),
            Self::UnknownSlot(value) => write!(
                f,
                "kernel command line has {KEY_SLOT}={value}, which is not a known slot"
            ),
            Self::MissingRootHash => write!(
                f,
                "kernel command line has no {KEY_ROOTHASH}; refusing to mount /usr \
                 unverified"
            ),
            Self::MalformedRootHash(value) => {
                write!(f, "{KEY_ROOTHASH}={value} is not a valid verity root hash")
            }
        }
    }
}

impl std::error::Error for CmdlineError {}

/// A root hash must be lowercase hex and long enough to be a real digest.
///
/// SHA-256 gives 64 characters, which is what image tooling produces today. The check
/// is a floor rather than an equality so that a future switch to a longer digest does
/// not require changing PID 1 — the hash is passed to the kernel verbatim regardless.
fn is_plausible_root_hash(value: &str) -> bool {
    value.len() >= 64
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

impl BootArgs {
    /// Parses a kernel command line.
    ///
    /// Unknown parameters are ignored: the command line also carries kernel options
    /// that are none of this module's business.
    ///
    /// `plexos.debug_shell` is parsed but only acted on by development builds. A
    /// production image must never let a command line parameter bypass service
    /// startup — though on a signed image an attacker cannot alter the command line
    /// anyway, since it lives inside the UKI.
    ///
    /// # Errors
    /// Fails if the slot or root hash is absent or malformed.
    pub fn parse(cmdline: &str) -> Result<Self, CmdlineError> {
        let mut slot = None;
        let mut root_hash = None;
        let mut debug_shell = false;

        for token in cmdline.split_whitespace() {
            let Some((key, value)) = token.split_once('=') else {
                if token == KEY_DEBUG_SHELL {
                    debug_shell = true;
                }
                continue;
            };
            match key {
                KEY_SLOT => slot = Some(value.to_owned()),
                KEY_ROOTHASH => root_hash = Some(value.to_owned()),
                KEY_DEBUG_SHELL => debug_shell = value != "0",
                _ => {}
            }
        }

        let slot_value = slot.ok_or(CmdlineError::MissingSlot)?;
        let slot = match slot_value.as_str() {
            "a" => Slot::A,
            "b" => Slot::B,
            _ => return Err(CmdlineError::UnknownSlot(slot_value)),
        };

        let root_hash = root_hash.ok_or(CmdlineError::MissingRootHash)?;
        if !is_plausible_root_hash(&root_hash) {
            return Err(CmdlineError::MalformedRootHash(root_hash));
        }

        Ok(Self {
            slot,
            root_hash,
            debug_shell,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn cmdline(extra: &str) -> String {
        format!("plexos.slot=a plexos.roothash={HASH} {extra}")
    }

    #[test]
    fn parses_a_real_looking_command_line() {
        let args = BootArgs::parse(&cmdline("quiet console=ttyS0,115200 rw")).unwrap();

        assert_eq!(args.slot, Slot::A);
        assert_eq!(args.root_hash, HASH);
        assert!(!args.debug_shell);
    }

    #[test]
    fn ignores_kernel_parameters_that_are_not_ours() {
        // The command line is shared with the kernel; unknown keys, valueless flags,
        // and duplicated options must not upset parsing.
        let args = BootArgs::parse(&cmdline("ro splash i915.enable_guc=3 initcall_debug")).unwrap();
        assert_eq!(args.slot, Slot::A);
    }

    #[test]
    fn reads_both_slots() {
        assert_eq!(
            BootArgs::parse(&format!("plexos.slot=b plexos.roothash={HASH}"))
                .unwrap()
                .slot,
            Slot::B
        );
    }

    #[test]
    fn refuses_to_boot_without_a_slot() {
        let err = BootArgs::parse(&format!("plexos.roothash={HASH}")).unwrap_err();
        assert_eq!(err, CmdlineError::MissingSlot);
        assert!(err.to_string().contains("image tooling"));
    }

    #[test]
    fn refuses_to_mount_usr_unverified() {
        // The load-bearing refusal: without a root hash there is no way to verify what
        // we would mount, so mounting anyway would defeat the whole trust chain.
        let err = BootArgs::parse("plexos.slot=a").unwrap_err();
        assert_eq!(err, CmdlineError::MissingRootHash);
        assert!(err.to_string().contains("unverified"));
    }

    #[test]
    fn rejects_a_root_hash_that_is_not_a_digest() {
        for bad in [
            "",
            "deadbeef",
            "9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08",
            "zzzz6d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        ] {
            let line = format!("plexos.slot=a plexos.roothash={bad}");
            assert!(
                matches!(
                    BootArgs::parse(&line),
                    Err(CmdlineError::MalformedRootHash(_) | CmdlineError::MissingRootHash)
                ),
                "should reject root hash {bad:?}"
            );
        }
    }

    #[test]
    fn accepts_a_longer_digest_than_sha256() {
        // Switching to a longer digest must not require changing PID 1: the hash is
        // handed to the kernel verbatim either way.
        let long = "a".repeat(128);
        let args = BootArgs::parse(&format!("plexos.slot=a plexos.roothash={long}")).unwrap();
        assert_eq!(args.root_hash, long);
    }

    #[test]
    fn rejects_an_unknown_slot_by_name() {
        let err = BootArgs::parse(&format!("plexos.slot=c plexos.roothash={HASH}")).unwrap_err();
        assert_eq!(err, CmdlineError::UnknownSlot("c".to_owned()));
        assert!(err.to_string().contains("plexos.slot=c"));
    }

    #[test]
    fn recognises_the_debug_shell_flag_in_both_forms() {
        assert!(
            BootArgs::parse(&cmdline("plexos.debug_shell"))
                .unwrap()
                .debug_shell
        );
        assert!(
            BootArgs::parse(&cmdline("plexos.debug_shell=1"))
                .unwrap()
                .debug_shell
        );
        assert!(
            !BootArgs::parse(&cmdline("plexos.debug_shell=0"))
                .unwrap()
                .debug_shell
        );
    }
}
