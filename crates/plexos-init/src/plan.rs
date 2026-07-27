//! The boot plan.
//!
//! `plexos-init` computes the complete sequence of operations *before* performing any
//! of them. The computation is a pure function of the kernel command line and the state
//! found on disk, which buys two things that matter a great deal for PID 1:
//!
//! - It is exhaustively testable with no filesystem, no root, and no device.
//! - It gives a `--dry-run` that prints exactly what would happen. Debugging PID 1 on a
//!   machine that will not boot is otherwise a matter of reading kernel panics.
//!
//! # Where this code runs
//!
//! From the initrd section of the Unified Kernel Image. The UKI is a single signed PE
//! binary containing kernel, initrd, and command line together, so there is no separate
//! initramfs artifact to sign or verify — the trust chain in ADR-0004 is unchanged, and
//! the root hash on the command line is covered by the same signature.
//!
//! The job here is to assemble the real root at [`SYSROOT`] and switch into it:
//! verified `/usr` from the running slot, persistent `/var`, an `/etc` overlay, and a
//! tmpfs for everything else.

use std::fmt;

use plexos_types::{Slot, paths};

use crate::cmdline::BootArgs;
use crate::state::StateAction;

/// Where the real root is assembled before switching into it.
pub const SYSROOT: &str = "/sysroot";

/// Device-mapper name for the verified `/usr` image.
pub const VERITY_MAPPER_NAME: &str = "plexos-usr";

/// The service manager `switch_root` hands control to.
pub const REAL_INIT: &str = "/usr/bin/plexos-init";

/// One operation in the boot sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootStep {
    /// Mount a kernel pseudo-filesystem.
    MountPseudo {
        /// Filesystem type, e.g. `proc`.
        fstype: &'static str,
        /// Where to mount it.
        target: String,
        /// Mount options.
        options: &'static str,
    },
    /// Create a directory, with parents.
    CreateDir {
        /// Path to create.
        path: String,
    },
    /// Set up dm-verity over a slot's `/usr` image.
    ///
    /// Every block is verified against the Merkle tree on read, for as long as the
    /// device exists — not once at mount time.
    SetupVerity {
        /// Partition holding the image.
        data_device: String,
        /// Partition holding the hash tree.
        hash_device: String,
        /// Root hash from the signed command line.
        root_hash: String,
        /// Resulting device-mapper name.
        mapper_name: &'static str,
    },
    /// Mount a filesystem.
    Mount {
        /// Device or source.
        source: String,
        /// Mount point.
        target: String,
        /// Filesystem type.
        fstype: &'static str,
        /// Mount options.
        options: &'static str,
    },
    /// Assemble `/etc` from factory defaults plus persistent changes.
    MountOverlay {
        /// Read-only defaults from the image.
        lower: String,
        /// Persistent changes on `/var`.
        upper: String,
        /// Overlayfs work directory, which must share a filesystem with `upper`.
        work: String,
        /// Where the assembled result appears.
        target: String,
    },
    /// Reconcile the persistent state layout (ADR-0009).
    ApplyState(StateAction),
    /// Move an already-mounted filesystem into the new root.
    MoveMount {
        /// Current mount point.
        from: String,
        /// Destination inside the new root.
        to: String,
    },
    /// Replace the early root with the assembled one and exec the real init.
    SwitchRoot {
        /// The assembled root.
        new_root: &'static str,
        /// Program to exec.
        init: &'static str,
    },
}

impl fmt::Display for BootStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MountPseudo {
                fstype,
                target,
                options,
            } => write!(f, "mount -t {fstype} -o {options} {fstype} {target}"),
            Self::CreateDir { path } => write!(f, "mkdir -p {path}"),
            Self::SetupVerity {
                data_device,
                hash_device,
                root_hash,
                mapper_name,
            } => write!(
                f,
                "veritysetup open {data_device} {mapper_name} {hash_device} {root_hash}"
            ),
            Self::Mount {
                source,
                target,
                fstype,
                options,
            } => write!(f, "mount -t {fstype} -o {options} {source} {target}"),
            Self::MountOverlay {
                lower,
                upper,
                work,
                target,
            } => write!(
                f,
                "mount -t overlay -o lowerdir={lower},upperdir={upper},workdir={work} \
                 overlay {target}"
            ),
            Self::ApplyState(action) => write!(f, "state: {action}"),
            Self::MoveMount { from, to } => write!(f, "mount --move {from} {to}"),
            Self::SwitchRoot { new_root, init } => write!(f, "switch_root {new_root} {init}"),
        }
    }
}

/// Partition device path for a slot's `/usr` image.
#[must_use]
pub fn usr_device(slot: Slot) -> String {
    format!("/dev/disk/by-partlabel/{}", slot.usr_label())
}

/// Partition device path for a slot's verity hash tree.
#[must_use]
pub fn verity_device(slot: Slot) -> String {
    format!("/dev/disk/by-partlabel/{}", slot.verity_label())
}

fn under_sysroot(path: &str) -> String {
    format!("{SYSROOT}{path}")
}

/// Builds the complete boot sequence.
///
/// The ordering constraints are not stylistic, and each is asserted by a test:
///
/// - verity is set up before anything mounts from it;
/// - `/var` is mounted before the state layout is touched;
/// - the `/etc` overlay comes after both `/usr` (its lower layer) and `/var` (its
///   upper layer) are available;
/// - `switch_root` is last, and nothing follows it.
#[must_use]
pub fn boot_plan(args: &BootArgs, state: StateAction) -> Vec<BootStep> {
    let mut steps = Vec::new();

    // Pseudo-filesystems first: everything below needs /dev and /proc.
    for (fstype, target, options) in [
        ("devtmpfs", "/dev", "nosuid,mode=0755"),
        ("proc", "/proc", "nosuid,nodev,noexec"),
        ("sysfs", "/sys", "nosuid,nodev,noexec"),
        ("tmpfs", "/run", "nosuid,nodev,mode=0755"),
    ] {
        steps.push(BootStep::MountPseudo {
            fstype,
            target: target.to_owned(),
            options,
        });
    }

    // The new root is a tmpfs: nothing outside /var survives a reboot, and that is
    // structural rather than a convention anyone has to remember.
    steps.push(BootStep::CreateDir {
        path: SYSROOT.to_owned(),
    });
    steps.push(BootStep::Mount {
        source: "tmpfs".to_owned(),
        target: SYSROOT.to_owned(),
        fstype: "tmpfs",
        options: "nosuid,nodev,mode=0755",
    });

    // Verify /usr before mounting it. A failure here is fatal by design: falling back
    // to an unverified mount would defeat the entire trust chain, so the boot fails and
    // the try counter in ADR-0005 hands the next boot to the other slot.
    steps.push(BootStep::SetupVerity {
        data_device: usr_device(args.slot),
        hash_device: verity_device(args.slot),
        root_hash: args.root_hash.clone(),
        mapper_name: VERITY_MAPPER_NAME,
    });

    let usr_target = under_sysroot(paths::USR);
    steps.push(BootStep::CreateDir {
        path: usr_target.clone(),
    });
    steps.push(BootStep::Mount {
        source: format!("/dev/mapper/{VERITY_MAPPER_NAME}"),
        target: usr_target,
        fstype: "erofs",
        options: "ro,nodev,nosuid",
    });

    // /var carries executable app images (ADR-0007), so nosuid,nodev is not optional.
    let var_target = under_sysroot(paths::VAR);
    steps.push(BootStep::CreateDir {
        path: var_target.clone(),
    });
    steps.push(BootStep::Mount {
        source: format!(
            "/dev/disk/by-partlabel/{}",
            plexos_types::partition::LABEL_VAR
        ),
        target: var_target,
        fstype: "xfs",
        options: "rw,nosuid,nodev",
    });

    // Only now can the state layout be inspected: it lives on /var.
    steps.push(BootStep::ApplyState(state));

    // /etc: factory defaults from the read-only image, persistent changes from /var.
    let etc_upper = under_sysroot(paths::PLEXOS_ETC);
    let etc_work = under_sysroot("/var/lib/plexos/.etc-work");
    steps.push(BootStep::CreateDir {
        path: etc_upper.clone(),
    });
    steps.push(BootStep::CreateDir {
        path: etc_work.clone(),
    });
    steps.push(BootStep::MountOverlay {
        lower: under_sysroot(paths::ETC_FACTORY),
        upper: etc_upper,
        work: etc_work,
        target: under_sysroot("/etc"),
    });

    // Carry the pseudo-filesystems across rather than remounting them, so nothing
    // holding an open file descriptor is disturbed.
    for mount in ["/dev", "/proc", "/sys", "/run"] {
        steps.push(BootStep::MoveMount {
            from: mount.to_owned(),
            to: under_sysroot(mount),
        });
    }

    steps.push(BootStep::SwitchRoot {
        new_root: SYSROOT,
        init: REAL_INIT,
    });

    steps
}

/// Renders a plan as the shell-like transcript shown by `--dry-run`.
#[must_use]
pub fn render(steps: &[BootStep]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for (index, step) in steps.iter().enumerate() {
        // Writing into a String cannot fail.
        let _ = writeln!(out, "{:>2}. {step}", index + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_types::version::STATE_LAYOUT_VERSION;

    const HASH: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn args(slot: Slot) -> BootArgs {
        BootArgs {
            slot,
            root_hash: HASH.to_owned(),
            debug_shell: false,
        }
    }

    fn plan_for(slot: Slot) -> Vec<BootStep> {
        boot_plan(&args(slot), StateAction::Proceed)
    }

    fn position<F: Fn(&BootStep) -> bool>(steps: &[BootStep], predicate: F) -> usize {
        steps
            .iter()
            .position(predicate)
            .expect("step not present in plan")
    }

    #[test]
    fn verity_is_established_before_anything_mounts_from_it() {
        let steps = plan_for(Slot::A);
        let verity = position(&steps, |s| matches!(s, BootStep::SetupVerity { .. }));
        let usr_mount = position(
            &steps,
            |s| matches!(s, BootStep::Mount { source, .. } if source.contains(VERITY_MAPPER_NAME)),
        );
        assert!(
            verity < usr_mount,
            "the trust chain requires verity before the mount that uses it"
        );
    }

    #[test]
    fn var_is_mounted_before_its_state_layout_is_touched() {
        let steps = plan_for(Slot::A);
        let var_mount = position(
            &steps,
            |s| matches!(s, BootStep::Mount { target, .. } if target.ends_with("/var")),
        );
        let state = position(&steps, |s| matches!(s, BootStep::ApplyState(_)));
        assert!(var_mount < state, "STATE_VERSION lives on /var");
    }

    #[test]
    fn the_etc_overlay_comes_after_both_of_its_layers_exist() {
        let steps = plan_for(Slot::A);
        let usr = position(
            &steps,
            |s| matches!(s, BootStep::Mount { target, .. } if target.ends_with("/usr")),
        );
        let var = position(
            &steps,
            |s| matches!(s, BootStep::Mount { target, .. } if target.ends_with("/var")),
        );
        let overlay = position(&steps, |s| matches!(s, BootStep::MountOverlay { .. }));

        assert!(usr < overlay, "lower layer comes from /usr");
        assert!(var < overlay, "upper layer lives on /var");
    }

    #[test]
    fn the_overlay_work_directory_shares_a_filesystem_with_its_upper_layer() {
        // An overlayfs requirement, and a silent failure if violated: the work
        // directory must be on the same filesystem as upperdir.
        let steps = plan_for(Slot::A);
        let BootStep::MountOverlay { upper, work, .. } = steps
            .iter()
            .find(|s| matches!(s, BootStep::MountOverlay { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        let var_prefix = under_sysroot("/var/");
        assert!(upper.starts_with(&var_prefix), "upper: {upper}");
        assert!(work.starts_with(&var_prefix), "work: {work}");
    }

    #[test]
    fn switch_root_is_last_and_nothing_follows_it() {
        let steps = plan_for(Slot::B);
        assert!(matches!(
            steps.last(),
            Some(BootStep::SwitchRoot {
                new_root: SYSROOT,
                init: REAL_INIT
            })
        ));
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s, BootStep::SwitchRoot { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn pseudo_filesystems_are_moved_across_rather_than_remounted() {
        let steps = plan_for(Slot::A);
        for mount in ["/dev", "/proc", "/sys", "/run"] {
            assert!(
                steps.iter().any(|s| matches!(
                    s,
                    BootStep::MoveMount { from, .. } if from == mount
                )),
                "{mount} is not carried into the new root"
            );
        }
        let first_move = position(&steps, |s| matches!(s, BootStep::MoveMount { .. }));
        let switch = position(&steps, |s| matches!(s, BootStep::SwitchRoot { .. }));
        assert!(first_move < switch);
    }

    #[test]
    fn each_slot_reads_its_own_partitions() {
        for slot in [Slot::A, Slot::B] {
            let steps = plan_for(slot);
            let BootStep::SetupVerity {
                data_device,
                hash_device,
                root_hash,
                ..
            } = steps
                .iter()
                .find(|s| matches!(s, BootStep::SetupVerity { .. }))
                .unwrap()
            else {
                unreachable!()
            };
            assert!(data_device.ends_with(slot.usr_label()), "{data_device}");
            assert!(hash_device.ends_with(slot.verity_label()), "{hash_device}");
            assert_eq!(
                root_hash, HASH,
                "the hash comes from the signed command line"
            );
        }

        assert_ne!(plan_for(Slot::A), plan_for(Slot::B));
    }

    #[test]
    fn writable_mounts_forbid_setuid_and_device_nodes() {
        // /var holds app images (ADR-0007), so this is a real boundary, not hygiene.
        let steps = plan_for(Slot::A);
        for step in &steps {
            if let BootStep::Mount {
                target, options, ..
            } = step
                && (target.ends_with("/var") || target.ends_with("/usr"))
            {
                assert!(options.contains("nosuid"), "{target}: {options}");
                assert!(options.contains("nodev"), "{target}: {options}");
            }
        }
    }

    #[test]
    fn usr_is_mounted_read_only() {
        let steps = plan_for(Slot::A);
        let BootStep::Mount { options, .. } = steps
            .iter()
            .find(|s| matches!(s, BootStep::Mount { target, .. } if target.ends_with("/usr")))
            .unwrap()
        else {
            unreachable!()
        };
        assert!(
            options.starts_with("ro"),
            "/usr must be read-only: {options}"
        );
    }

    #[test]
    fn every_mount_point_is_created_before_it_is_used() {
        let steps = plan_for(Slot::A);
        for (index, step) in steps.iter().enumerate() {
            let target = match step {
                BootStep::Mount { target, .. } if target != SYSROOT => target,
                _ => continue,
            };
            let created = steps[..index]
                .iter()
                .any(|s| matches!(s, BootStep::CreateDir { path } if path == target));
            assert!(created, "{target} is mounted before it is created");
        }
    }

    #[test]
    fn the_state_action_is_carried_into_the_plan_verbatim() {
        let action = StateAction::RestoreForRollback {
            found: 5,
            expected: STATE_LAYOUT_VERSION,
        };
        let steps = boot_plan(&args(Slot::A), action);
        assert!(steps.contains(&BootStep::ApplyState(action)));
    }

    #[test]
    fn dry_run_output_is_readable_and_numbered() {
        let text = render(&plan_for(Slot::A));
        assert!(text.contains("veritysetup open"), "{text}");
        assert!(text.contains("switch_root"), "{text}");
        assert!(
            text.contains(HASH),
            "the root hash should be visible: {text}"
        );
        assert_eq!(text.lines().count(), plan_for(Slot::A).len());
    }
}
