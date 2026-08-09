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

/// The merged-`/usr` compatibility symlinks, as `(link, target)`.
///
/// A merged `/usr` is what makes `/usr` the unit of update at all (ADR-0001), and
/// these four links are the other half of that arrangement: every `/bin/...` and
/// `/lib/...` path in the system resolves through them. Buildroot creates them in its
/// target root, but the root PlexOS boots is a tmpfs assembled here, so they have to
/// be recreated — nothing carries them across.
///
/// Omitting them produces a system that mounts perfectly and then cannot execute
/// `/bin/sh`, which is how the first image to reach the service manager failed.
pub const MERGED_USR_LINKS: [(&str, &str); 4] = [
    ("/bin", "usr/bin"),
    ("/sbin", "usr/sbin"),
    ("/lib", "usr/lib"),
    ("/lib64", "lib"),
];

/// Device-mapper name for the verified `/usr` image.
pub const VERITY_MAPPER_NAME: &str = "plexos-usr";

/// The service manager `switch_root` hands control to.
pub const REAL_INIT: &str = "/usr/bin/plexos-init";

/// Passed to [`REAL_INIT`] so it knows it is the second of the two roles described in
/// ARCHITECTURE.md §3, and must supervise rather than assemble a root that already
/// exists.
///
/// Without it the second invocation reads the same command line, computes the same
/// plan, and executes it again. The first image that booted this far did exactly
/// that, and failed at verity with `EBUSY` on a device it had itself just created.
pub const SUPERVISE_FLAG: &str = "--supervise";

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
    /// Create a symbolic link.
    Symlink {
        /// What the link points at.
        target: &'static str,
        /// Where the link is created.
        link: String,
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
    /// Raise the system clock to the image's build time if it reads earlier (see
    /// [`crate::clock`]).
    ///
    /// Placed after `/usr` is mounted because the build stamp lives inside it, and
    /// deliberately before anything that speaks TLS: a clock corrected later is a clock
    /// corrected after the first handshake has already failed.
    RaiseClock {
        /// The image's `os-release`, under the sysroot rather than at `/`, because the
        /// switch has not happened yet when this runs.
        os_release: String,
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
            Self::Symlink { target, link } => write!(f, "ln -s {target} {link}"),
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
            Self::RaiseClock { os_release } => {
                write!(
                    f,
                    "clock: raise to the build time in {os_release} if behind"
                )
            }
            Self::ApplyState(action) => write!(f, "state: {action}"),
            Self::MoveMount { from, to } => write!(f, "mount --move {from} {to}"),
            Self::SwitchRoot { new_root, init } => write!(f, "switch_root {new_root} {init}"),
        }
    }
}

/// The step that keeps the clock from being absurd, once `/usr` can be read.
///
/// A machine whose CMOS battery has died boots believing it is 1970, and a machine that
/// believes that cannot verify a single certificate on the internet. It presents as Plex
/// failing to download with a message about a *certificate*, which sends the reader
/// somewhere the fault is not. The image knows when it was built; that is the floor.
fn clock_step() -> BootStep {
    BootStep::RaiseClock {
        os_release: under_sysroot("/usr/lib/os-release"),
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
    //
    // cgroup2 is in the list because nothing else mounts it, and its absence does not
    // look like a missing mount: ADR-0007 bounds Plex with cgroup v2, and
    // plexos_plex::cgroup::apply fails to create its directory, so Plex does not start
    // at all. It comes after sysfs because /sys/fs/cgroup is a directory *in* sysfs and
    // does not exist before it. It is carried into the new root by the MS_MOVE of /sys
    // below, which relocates the whole subtree beneath a mount rather than only its top.
    //
    // devpts is here for exactly the same reason, and was found the same way -- by the
    // thing that needed it failing on hardware. devtmpfs provides the /dev/ptmx device
    // node, so opening it succeeds; what it does not provide is the /dev/pts directory
    // the allocated slave appears in, and without that openpty(3) fails with ENOENT. The
    // symptom is a terminal that reports it cannot start a shell, naming a shell that
    // exists and was never reached. It comes after /dev because /dev/pts is a directory
    // in it.
    //
    // gid=5,mode=0620 is the conventional tty-group ownership for slaves; ptmxmode=0666
    // makes /dev/pts/ptmx usable, which matters if anything ever opens that rather than
    // the devtmpfs node.
    for (fstype, target, options) in [
        ("devtmpfs", "/dev", "nosuid,mode=0755"),
        (
            "devpts",
            "/dev/pts",
            "nosuid,noexec,gid=5,mode=0620,ptmxmode=0666",
        ),
        ("proc", "/proc", "nosuid,nodev,noexec"),
        ("sysfs", "/sys", "nosuid,nodev,noexec"),
        ("cgroup2", "/sys/fs/cgroup", "nosuid,nodev,noexec"),
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

    steps.push(clock_step());

    // The compatibility links, created on the tmpfs root now that /usr is under it.
    // Relative targets, so they resolve correctly both here under /sysroot and after
    // switch_root has made it /.
    for (link, target) in MERGED_USR_LINKS {
        steps.push(BootStep::Symlink {
            target,
            link: under_sysroot(link),
        });
    }

    // /tmp, which nothing else creates. The root is a tmpfs assembled here from
    // nothing, so the /tmp in the Buildroot rootfs never reaches the running system,
    // and its absence stays invisible until something calls mktemp. busybox then
    // reports the directory it could not use rather than the one it wanted, giving
    // `mktemp: : No such file or directory` — a message with an empty path in it that
    // says nothing about /tmp. udhcpc's lease script hit exactly this and lost the
    // machine its DNS configuration.
    //
    // A mount rather than a plain directory, so it can carry mode=1777. ADR-0007 runs
    // Plex unprivileged, and a 0755 /tmp owned by root is useless to it.
    let tmp_target = under_sysroot("/tmp");
    steps.push(BootStep::CreateDir {
        path: tmp_target.clone(),
    });
    steps.push(BootStep::Mount {
        source: "tmpfs".to_owned(),
        target: tmp_target,
        fstype: "tmpfs",
        options: "nosuid,nodev,mode=1777",
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
    fn devpts_is_mounted_or_no_terminal_can_ever_be_opened() {
        // Found on hardware, and it looks like something else entirely: devtmpfs gives
        // /dev/ptmx, so opening it works, and openpty then fails with ENOENT because the
        // slave appears under /dev/pts and nothing mounted one. The console reported that
        // it could not start /bin/sh -- a shell that exists and was never reached.
        //
        // Same shape as cgroup2 two entries above it: the running root contains only what
        // this plan puts there.
        let plan = boot_plan(&args(Slot::A), StateAction::Proceed);

        let devpts = plan.iter().position(
            |s| matches!(s, BootStep::MountPseudo { fstype: "devpts", target, .. } if target == "/dev/pts"),
        );
        let dev = plan.iter().position(|s| {
            matches!(
                s,
                BootStep::MountPseudo {
                    fstype: "devtmpfs",
                    ..
                }
            )
        });

        let devpts = devpts.expect("devpts must be mounted; openpty needs /dev/pts");
        let dev = dev.expect("devtmpfs is mounted");
        assert!(
            dev < devpts,
            "/dev/pts is a directory in /dev, so it cannot be mounted first"
        );
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
    fn the_clock_is_raised_after_usr_is_mounted_and_before_the_switch() {
        // Both halves matter and both are ordering, which is the kind of mistake that
        // compiles. Earlier than the mount and the build stamp is not readable, so the
        // step silently does nothing on every boot. Later than switch_root and there is
        // no step at all -- and the window it is protecting is the whole of userspace,
        // because the first thing that speaks TLS has already failed by then.
        let steps = plan_for(Slot::A);
        let usr = position(
            &steps,
            |s| matches!(s, BootStep::Mount { target, .. } if target.ends_with("/usr")),
        );
        let clock = position(&steps, |s| matches!(s, BootStep::RaiseClock { .. }));
        let switch = position(&steps, |s| matches!(s, BootStep::SwitchRoot { .. }));

        assert!(
            usr < clock,
            "the build stamp is inside /usr and it is not mounted yet"
        );
        assert!(clock < switch, "the clock step never runs");
    }

    #[test]
    fn the_clock_step_reads_the_image_under_the_sysroot() {
        // The path is resolved before switch_root, so a bare /usr/lib/os-release would
        // read the *initrd's* copy -- which on this image does not exist, so the step
        // would report an unreadable file forever and nobody would look again.
        let steps = plan_for(Slot::A);
        let BootStep::RaiseClock { os_release } = steps
            .iter()
            .find(|s| matches!(s, BootStep::RaiseClock { .. }))
            .expect("the plan raises the clock")
        else {
            unreachable!()
        };
        assert_eq!(os_release, &under_sysroot("/usr/lib/os-release"));
    }

    #[test]
    fn the_merged_usr_links_are_created_after_usr_is_mounted() {
        // They point into /usr, so creating them earlier would produce dangling
        // links; the system would still boot and then fail to run anything.
        let steps = plan_for(Slot::A);
        let usr = position(
            &steps,
            |s| matches!(s, BootStep::Mount { target, .. } if target.ends_with("/usr")),
        );
        for (link, _) in MERGED_USR_LINKS {
            let expected = under_sysroot(link);
            let at = position(
                &steps,
                |s| matches!(s, BootStep::Symlink { link, .. } if *link == expected),
            );
            assert!(at > usr, "{link} is created before /usr is mounted");
        }
    }

    #[test]
    fn the_merged_usr_link_targets_are_relative() {
        // An absolute target would point at the *initrd's* /usr while the plan runs
        // under /sysroot, and the links would silently resolve to the wrong tree.
        for (_, target) in MERGED_USR_LINKS {
            assert!(!target.starts_with('/'), "{target} must be relative");
        }
    }

    #[test]
    fn every_link_needed_to_execute_a_binary_is_present() {
        // /bin/sh is what the service manager starts, and shebang lines and the
        // dynamic loader path both resolve through these.
        let links: Vec<&str> = MERGED_USR_LINKS.iter().map(|(l, _)| *l).collect();
        for required in ["/bin", "/sbin", "/lib", "/lib64"] {
            assert!(links.contains(&required), "{required} link missing");
        }
    }

    #[test]
    fn the_plan_mounts_cgroup_v2_because_nothing_else_does() {
        // Its absence does not look like a missing mount. ADR-0007 bounds Plex with
        // cgroup v2, so plexos_plex::cgroup::apply cannot create its directory and Plex
        // does not start at all -- on a machine whose kernel has every controller.
        let steps = plan_for(Slot::A);
        let cgroup = steps.iter().position(|step| {
            matches!(step, BootStep::MountPseudo { fstype, target, .. }
                if *fstype == "cgroup2" && target == "/sys/fs/cgroup")
        });
        let sysfs = steps.iter().position(
            |step| matches!(step, BootStep::MountPseudo { fstype, .. } if *fstype == "sysfs"),
        );

        let cgroup = cgroup.unwrap_or_else(|| panic!("the plan must mount cgroup2: {steps:?}"));
        let sysfs = sysfs.expect("the plan must mount sysfs");
        assert!(
            sysfs < cgroup,
            "/sys/fs/cgroup is a directory in sysfs and does not exist before it"
        );
    }

    #[test]
    fn the_root_carries_a_writable_tmp() {
        // The root is a tmpfs built here from nothing, so every directory on it is one
        // this plan put there. /tmp was not among them, and nothing said so: the first
        // symptom was `mktemp: : No such file or directory` out of udhcpc's lease
        // script -- a message naming an empty path, from a program that had been asked
        // for a temporary file in a directory that did not exist.
        let steps = plan_for(Slot::A);
        let tmp = under_sysroot("/tmp");

        assert!(
            steps.iter().any(|s| matches!(
                s,
                BootStep::Mount { target, fstype, .. } if *target == tmp && *fstype == "tmpfs"
            )),
            "the plan must put a tmpfs on {tmp}: {steps:?}"
        );
    }

    #[test]
    fn tmp_is_world_writable_and_sticky() {
        // ADR-0007 runs Plex unprivileged. A /tmp at the default 0755 owned by root is
        // present, passes the test above, and is still unusable to the one process this
        // appliance exists to run.
        let steps = plan_for(Slot::A);
        let tmp = under_sysroot("/tmp");
        let options = steps
            .iter()
            .find_map(|s| match s {
                BootStep::Mount {
                    target, options, ..
                } if *target == tmp => Some(*options),
                _ => None,
            })
            .expect("a /tmp mount");

        assert!(options.contains("mode=1777"), "got {options:?}");
        assert!(options.contains("nosuid"), "got {options:?}");
        assert!(options.contains("nodev"), "got {options:?}");
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
