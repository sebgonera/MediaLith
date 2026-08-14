//! Performing the boot plan.
//!
//! [`crate::plan`] computes what to do as a pure function; this module does it. The
//! split is the whole reason the hard part is testable: everything about *which*
//! steps happen, and in what order, is decided and asserted without a filesystem, a
//! device, or root. What is left here is the narrow business of making each step
//! actually happen.
//!
//! # Failure is fatal, deliberately
//!
//! No step here recovers. A verity failure does not fall back to an unverified mount
//! — that would defeat the entire trust chain (ADR-0004). A failed mount does not
//! continue with a partially assembled root. `plexos-init` exits, the kernel panics
//! on a dead PID 1, and the boot counter in ADR-0005 hands the next attempt to the
//! other slot. That is the designed behaviour, not a gap.
//!
//! The one thing that must not happen is failing *silently*, or failing after the
//! point of no return. [`plexos_sys::mount::switch_root`] therefore checks that the
//! new root contains an init before it moves anything, because after `MS_MOVE` there
//! is no way back and the resulting panic names nothing useful.

use std::convert::Infallible;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use plexos_sys::{dm, mount, verity::VeritySuperblock};
use plexos_types::paths;

use crate::plan::BootStep;
use crate::state::StateAction;

/// A step that could not be performed.
#[derive(Debug)]
pub struct ExecError {
    /// The step that failed, rendered as the dry-run would show it.
    pub step: String,
    /// What went wrong.
    pub source: io::Error,
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "boot step failed: {}\n  {}", self.step, self.source)
    }
}

impl std::error::Error for ExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Somewhere to send progress, so the executor can be exercised without a console.
pub trait Log {
    /// Records one line.
    fn line(&mut self, text: &str);
}

/// Writes progress to stderr, which on a real boot is the kernel console.
pub struct StderrLog;

impl Log for StderrLog {
    fn line(&mut self, text: &str) {
        eprintln!("plexos-init: {text}");
    }
}

fn io_error(kind: io::ErrorKind, message: String) -> io::Error {
    io::Error::new(kind, message)
}

/// Reads the verity superblock from the front of a hash device.
fn read_superblock(hash_device: &str) -> io::Result<VeritySuperblock> {
    let bytes = {
        use std::io::Read as _;
        let mut file = fs::File::open(hash_device).map_err(|error| {
            io_error(
                error.kind(),
                format!("opening verity hash device {hash_device}: {error}"),
            )
        })?;
        let mut buffer = vec![0u8; plexos_sys::verity::SUPERBLOCK_BYTES];
        file.read_exact(&mut buffer).map_err(|error| {
            io_error(
                error.kind(),
                format!("reading verity superblock from {hash_device}: {error}"),
            )
        })?;
        buffer
    };

    VeritySuperblock::parse(&bytes)
        .map_err(|error| io_error(io::ErrorKind::InvalidData, error.to_string()))
}

/// Performs the state action decided in [`crate::state`].
///
/// Rollback reverts `/usr`, never `/var` (ADR-0009), so anything done here has to
/// leave state the previous release can still read.
fn apply_state(action: StateAction, log: &mut dyn Log) -> io::Result<()> {
    let sysroot_state = format!("{}{}", crate::plan::SYSROOT, paths::PLEXOS_STATE);
    let version_file = format!("{}{}", crate::plan::SYSROOT, paths::STATE_VERSION_FILE);

    match action {
        StateAction::Proceed => Ok(()),

        StateAction::Initialise { to } => {
            log.line(&format!("initialising /var at layout version {to}"));
            fs::create_dir_all(&sysroot_state)?;
            fs::write(&version_file, format!("{to}\n"))
        }

        StateAction::Migrate { from, to } => {
            // Unreachable today: STATE_LAYOUT_VERSION has only ever been 1, so this
            // needs a /var stamped 0. When migrations do exist they belong here, each
            // backed up to paths::BACKUP first (ADR-0009).
            //
            // Older state is fatal where newer state is not, and the asymmetry is
            // deliberate: newer state means a rollback, which must boot. Older state
            // with no migration means this release would be interpreting a layout it
            // does not understand, and /var is the only thing on the disk that cannot
            // be recreated.
            Err(io_error(
                io::ErrorKind::Unsupported,
                format!(
                    "no migration from /var layout version {from} to {to}; \
                     refusing to interpret state this release does not understand. \
                     Boot a release that implements version {from}, or reinstall."
                ),
            ))
        }

        StateAction::RestoreForRollback { found, expected } => {
            // Never fatal, and this is the single most important line in the module.
            // Newer state means we have rolled back — the safety mechanism working as
            // intended. Refusing to boot here would make that mechanism the thing
            // that bricks the machine, which is exactly the trap state.rs was written
            // to avoid. Log loudly, and continue.
            //
            // Restoring pre-migration backups from paths::BACKUP belongs here too,
            // and is a no-op until a migration exists to have written one.
            log.line(&format!(
                "ROLLBACK: /var is at layout version {found}, this release implements \
                 {expected}. Continuing with compatible state; some settings written \
                 by the newer release may be ignored."
            ));
            Ok(())
        }
    }
}

/// Whether `path` is a mount point, without consulting `/proc`.
///
/// A mounted directory sits on a different device from its parent, so comparing the
/// two `st_dev` values answers the question. Reading `/proc/self/mountinfo` would be
/// the usual approach and cannot be used here: this is called to decide whether
/// `/proc` itself is mounted.
fn is_mounted(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(target) = fs::metadata(path) else {
        return false;
    };
    let Some(parent) = Path::new(path).parent() else {
        return false;
    };
    let Ok(parent) = fs::metadata(parent) else {
        return false;
    };
    target.dev() != parent.dev()
}

/// Mounts `/proc` so the kernel command line can be read.
///
/// This exists because of an ordering problem that only appears on a real boot, and
/// did: the boot plan is a pure function of the kernel command line, the command line
/// is read from `/proc/cmdline`, and mounting `/proc` is a step *in* the plan. The
/// first image built panicked with "could not read /proc/cmdline: No such file or
/// directory" for exactly this reason.
///
/// So a small bootstrap runs before planning. The plan still contains its own
/// `/proc` mount, because the plan describes a complete boot and should be readable
/// as such; `perform` skips any pseudo-filesystem already mounted rather than
/// stacking a second mount over it.
///
/// # Errors
///
/// If `/proc` can neither be created nor mounted, in which case the command line
/// cannot be read and there is nothing to plan from.
pub fn bootstrap_proc() -> io::Result<()> {
    if is_mounted("/proc") {
        return Ok(());
    }
    fs::create_dir_all("/proc")?;
    mount::mount("proc", "/proc", "proc", "nosuid,nodev,noexec")
}

/// Raises the system clock to the image's build time, if it reads earlier.
///
/// Reports what it did in every case. "The clock was already sane" is worth a line: the
/// alternative is that a machine with a dead battery and a machine with a good one look
/// identical from the console, and telling them apart afterwards means reading a
/// certificate error and inferring backwards.
fn raise_clock(os_release: &str, log: &mut dyn Log) {
    let Ok(contents) = fs::read_to_string(os_release) else {
        log.line(&format!(
            "clock: {os_release} is unreadable, so the image's build time is unknown and \
             the clock is left as it is. Remedy: if TLS then fails with 'certificate is \
             not yet valid', set it by hand with `date -s` and `hwclock -w -u`"
        ));
        return;
    };

    let Some(floor) =
        crate::clock::os_release_value(&contents, "VERSION_ID").and_then(crate::clock::build_time)
    else {
        log.line(
            "clock: this image carries no build stamp in VERSION_ID, so there is no \
             floor to apply. Remedy: build with PLEXOS_VERSION=0.1.0.$(date -u \
             +%Y%m%d%H%M)",
        );
        return;
    };

    let now = plexos_sys::clock::realtime_now();
    let Some(corrected) = crate::clock::correction(now, floor) else {
        log.line(&format!(
            "clock: reads {now}, at or after this image's {floor}; left alone"
        ));
        return;
    };

    match plexos_sys::clock::set_realtime(corrected) {
        Ok(()) => log.line(&format!(
            "clock: read {now}, which is before this image was built; raised to {corrected}. \
             The hardware clock is wrong -- most likely a dead CMOS battery. It is not the \
             real time, only a plausible one, which is what TLS needs"
        )),
        Err(error) => log.line(&format!(
            "clock: could not raise {now} to {corrected}: {error}. Outbound TLS will fail \
             with 'certificate is not yet valid'. Remedy: `date -s` then `hwclock -w -u` \
             from the console shell"
        )),
    }
}

/// Performs one step.
fn perform(step: &BootStep, booted: Option<&str>, log: &mut dyn Log) -> io::Result<()> {
    match step {
        BootStep::MountPseudo {
            fstype,
            target,
            options,
        } => {
            // Already mounted by the bootstrap above, or by a previous attempt.
            // Mounting again would succeed and stack a second mount on the same
            // point, leaving MoveMount to carry only the top one into the new root.
            if is_mounted(target) {
                log.line(&format!("{target} is already mounted, skipping"));
                return Ok(());
            }
            fs::create_dir_all(target)?;
            mount::mount(fstype, target, fstype, options)
        }

        BootStep::CreateDir { path } => fs::create_dir_all(path),

        // Never fatal. A machine with a wrong clock boots, serves its console and can be
        // repaired; a machine that refuses to boot over one cannot. Every outcome is
        // logged, including the uninteresting ones, because the failure this prevents is
        // invisible from outside and its absence has to be visible from the console.
        BootStep::RaiseClock { os_release } => {
            raise_clock(os_release, log);
            Ok(())
        }

        BootStep::Symlink { target, link } => {
            // Idempotent: a retried boot, or a link the image already carries,
            // should not fail the whole sequence.
            match std::os::unix::fs::symlink(target, link) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                other => other,
            }
        }

        BootStep::SetupVerity {
            data_device,
            hash_device,
            root_hash,
            mapper_name,
        } => {
            // The plan names devices by GPT label, which is how ADR-0003 carries
            // slot identity. udev is what normally turns those into paths, and the
            // initrd has none, so resolve them here.
            let data_device =
                plexos_sys::device::resolve_on(booted, data_device, &mut |m| log.line(m))?;
            let hash_device =
                plexos_sys::device::resolve_on(booted, hash_device, &mut |m| log.line(m))?;
            let superblock = read_superblock(&hash_device)?;
            log.line(&format!(
                "verity: {} blocks of {} bytes, {}",
                superblock.data_blocks, superblock.data_block_size, superblock.algorithm
            ));
            log.line(&format!("verity: data {data_device}, hash {hash_device}"));
            let node = dm::create_verity(
                mapper_name,
                &data_device,
                &hash_device,
                root_hash,
                &superblock,
            )?;
            log.line(&format!("verity: {} is live", node.display()));
            Ok(())
        }

        BootStep::Mount {
            source,
            target,
            fstype,
            options,
        } => {
            let source = plexos_sys::device::resolve_on(booted, source, &mut |m| log.line(m))?;
            mount::mount(&source, target, fstype, options)
        }

        BootStep::MountOverlay {
            lower,
            upper,
            work,
            target,
        } => {
            fs::create_dir_all(target)?;
            let options = format!("lowerdir={lower},upperdir={upper},workdir={work}");
            mount::mount("overlay", target, "overlay", &options)
        }

        BootStep::ApplyState(action) => apply_state(*action, log),

        BootStep::MoveMount { from, to } => {
            fs::create_dir_all(to)?;
            mount::move_mount(from, to)
        }

        BootStep::SwitchRoot { new_root, init } => {
            // Never returns on success.
            mount::switch_root(new_root, init, &[crate::plan::SUPERVISE_FLAG]).map(|_| ())
        }
    }
}

/// Performs every step in order, stopping at the first failure.
///
/// Returns only on failure. The success path ends in `switch_root`, which replaces
/// this process image, so there is no value to return — hence [`Infallible`] as the
/// success type rather than `()`. That makes "this returned normally" a state the
/// compiler knows cannot happen, instead of one every caller has to remember to
/// handle.
///
/// # Errors
///
/// The first step that fails, with the step text included so the console says which
/// one rather than only what the errno was.
pub fn execute(steps: &[BootStep], log: &mut dyn Log) -> Result<Infallible, ExecError> {
    // Asked once, before anything is resolved. A machine that has installed MediaLith onto a
    // disk and still has the installer stick in it carries two partitions called `usr_a`
    // and two called `var`, and a label alone cannot say which pair belongs to the system
    // the firmware just booted. Mounting the wrong `/var` is the silent half of that: the
    // machine comes up with another installation's media database, device token and
    // certificate, and nothing reports anything wrong.
    //
    // `systemd-boot` knows, because it is what the firmware loaded, and it leaves the
    // answer in an EFI variable. `None` is not a failure -- it is what every machine with
    // one disk has always been, and the resolution falls back to what it did before.
    let booted = plexos_sys::device::booted_disk(&mut |m| log.line(m));

    for (index, step) in steps.iter().enumerate() {
        log.line(&format!("{:>2}/{} {step}", index + 1, steps.len()));
        perform(step, booted.as_deref(), log).map_err(|source| ExecError {
            step: step.to_string(),
            source,
        })?;
    }
    // Only reachable if the plan had no SwitchRoot, which boot_plan never produces.
    Err(ExecError {
        step: "end of plan".to_owned(),
        source: io_error(
            io::ErrorKind::Other,
            "the plan completed without switching root, so there is nothing to run".to_owned(),
        ),
    })
}

/// Whether this process is PID 1.
///
/// Used to refuse to execute a plan on a running system by accident: every step here
/// is destructive to the mount namespace, and doing it to a developer's workstation
/// is a bad way to find out the guard was missing.
#[must_use]
pub fn is_pid_one() -> bool {
    std::process::id() == 1
}

/// Whether the environment looks like the initrd this is meant to run in.
#[must_use]
pub fn looks_like_initrd() -> bool {
    !Path::new(crate::plan::SYSROOT).exists() || Path::new(dm::CONTROL).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::VERITY_MAPPER_NAME;
    use plexos_types::version::STATE_LAYOUT_VERSION;

    struct Collect(Vec<String>);

    impl Log for Collect {
        fn line(&mut self, text: &str) {
            self.0.push(text.to_owned());
        }
    }

    #[test]
    fn a_developer_workstation_is_not_pid_one() {
        // The guard that stops `plexos-init --execute` from dismantling the mount
        // namespace of the machine it is being developed on.
        assert!(!is_pid_one(), "the test runner should never be PID 1");
    }

    #[test]
    fn a_missing_hash_device_names_the_label_problem() {
        let error = read_superblock("/nonexistent-hash-device").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("/nonexistent-hash-device"), "{text}");
        assert!(text.contains("verity hash device"), "{text}");
    }

    #[test]
    fn a_device_that_is_not_verity_is_rejected_with_a_reason() {
        let mut file = std::env::temp_dir();
        file.push("plexos-not-verity.bin");
        fs::write(&file, vec![0u8; 1024]).unwrap();
        let error = read_superblock(file.to_str().unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("verity signature"), "{error}");
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn proceeding_state_does_nothing() {
        let mut log = Collect(Vec::new());
        apply_state(StateAction::Proceed, &mut log).unwrap();
        assert!(log.0.is_empty());
    }

    #[test]
    fn an_unknown_migration_stops_the_boot_rather_than_guessing() {
        // /var outlives the OS image, so using state this release does not understand
        // risks corrupting the only thing on the disk anyone cares about.
        let mut log = Collect(Vec::new());
        let error = apply_state(StateAction::Migrate { from: 1, to: 2 }, &mut log).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        let text = error.to_string();
        assert!(text.contains("no migration"), "{text}");
        assert!(
            text.contains("Boot a release that implements version 1"),
            "the refusal must name a way out: {text}"
        );
    }

    #[test]
    fn a_rollback_boots_rather_than_failing() {
        // The rule state.rs exists to enforce: finding newer state is NEVER fatal.
        // It means rollback worked, and refusing to boot would turn the safety
        // mechanism into the thing that bricks the machine. If this test ever starts
        // expecting an error, the change is wrong.
        let mut log = Collect(Vec::new());
        apply_state(
            StateAction::RestoreForRollback {
                found: 5,
                expected: STATE_LAYOUT_VERSION,
            },
            &mut log,
        )
        .expect("a rollback must boot");
    }

    #[test]
    fn a_rollback_is_logged_loudly_rather_than_passing_in_silence() {
        // "Log loudly, and boot" -- silently ignoring settings written by a newer
        // release is how a rollback becomes an unexplained loss of configuration.
        let mut log = Collect(Vec::new());
        apply_state(
            StateAction::RestoreForRollback {
                found: 5,
                expected: STATE_LAYOUT_VERSION,
            },
            &mut log,
        )
        .unwrap();
        assert_eq!(log.0.len(), 1);
        assert!(log.0[0].contains("ROLLBACK"), "{:?}", log.0[0]);
        assert!(log.0[0].contains("may be ignored"), "{:?}", log.0[0]);
    }

    #[test]
    fn execution_reports_which_step_failed_not_just_the_errno() {
        // "Operation not permitted" with no context is what makes debugging PID 1 on
        // a machine that will not boot so unpleasant.
        let steps = vec![BootStep::Mount {
            source: "/nonexistent".to_owned(),
            target: "/nonexistent-target".to_owned(),
            fstype: "ext4",
            options: "ro",
        }];
        let mut log = Collect(Vec::new());
        let error = execute(&steps, &mut log).unwrap_err();
        assert!(error.step.contains("mount"), "{}", error.step);
        assert!(error.to_string().contains("/nonexistent"), "{error}");
    }

    #[test]
    fn execution_stops_at_the_first_failure() {
        // Continuing past a failed mount would assemble a partial root and switch
        // into it, which is worse than not booting.
        let steps = vec![
            BootStep::Mount {
                source: "/nonexistent".to_owned(),
                target: "/nonexistent-target".to_owned(),
                fstype: "ext4",
                options: "ro",
            },
            BootStep::CreateDir {
                path: "/should-never-be-created-by-tests".to_owned(),
            },
        ];
        let mut log = Collect(Vec::new());
        assert!(execute(&steps, &mut log).is_err());
        assert!(!Path::new("/should-never-be-created-by-tests").exists());
        assert_eq!(
            announced(&log),
            1,
            "only the failing step should be announced"
        );
    }

    #[test]
    fn every_step_is_announced_before_it_is_attempted() {
        // On a boot that hangs, the last line printed is the step that hung. Logging
        // after the fact would print nothing at all in that case.
        let steps = vec![BootStep::Mount {
            source: "/nonexistent".to_owned(),
            target: "/nonexistent-target".to_owned(),
            fstype: "ext4",
            options: "ro",
        }];
        let mut log = Collect(Vec::new());
        let _ = execute(&steps, &mut log);
        assert_eq!(announced(&log), 1);
        assert!(log.0.iter().any(|l| l.starts_with(" 1/1 ")), "{:?}", log.0);
    }

    /// How many *steps* were announced.
    ///
    /// Counted rather than taking the whole log, because `execute` also reports what it
    /// found out about the machine before it starts — which disk the firmware booted, or
    /// why that could not be established. Asserting on the total made those two things one
    /// number, and the first line of a boot log is not a step.
    fn announced(log: &Collect) -> usize {
        log.0
            .iter()
            .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .count()
    }

    #[test]
    fn a_plan_without_switch_root_is_an_error_rather_than_a_success() {
        let mut log = Collect(Vec::new());
        let error = execute(&[], &mut log).unwrap_err();
        assert!(error.to_string().contains("nothing to run"), "{error}");
    }

    #[test]
    fn a_mounted_filesystem_is_recognised_without_reading_proc() {
        // /proc is mounted on any machine running these tests, and sits on a
        // different device from /. If this ever returns false, bootstrap_proc would
        // stack a second /proc over the first on every boot.
        assert!(is_mounted("/proc"), "/proc should be detected as a mount");
    }

    #[test]
    fn an_ordinary_directory_is_not_mistaken_for_a_mount() {
        // A false positive here is worse than a false negative: the pseudo-filesystem
        // would be skipped and never mounted at all.
        assert!(!is_mounted("/proc/self"), "/proc/self is not a mount point");
        assert!(!is_mounted("/nonexistent-path-xyz"));
    }

    #[test]
    fn an_already_mounted_pseudo_filesystem_is_skipped_not_stacked() {
        // The bootstrap mounts /proc before planning, so the plan's own /proc step
        // must become a no-op. Mounting again succeeds and stacks a second mount on
        // the same point, after which MoveMount carries only the top one across.
        let step = BootStep::MountPseudo {
            fstype: "proc",
            target: "/proc".to_owned(),
            options: "nosuid,nodev,noexec",
        };
        let mut log = Collect(Vec::new());
        perform(&step, None, &mut log).expect("skipping an existing mount must succeed");
        assert!(
            log.0.iter().any(|l| l.contains("already mounted")),
            "the skip should be visible on the console: {:?}",
            log.0
        );
    }

    #[test]
    fn the_verity_mapper_name_matches_what_the_plan_mounts() {
        // The plan mounts /dev/mapper/<name>; dm::create_verity creates that node.
        // If these ever drift, the mount fails with ENOENT on a working device.
        assert_eq!(VERITY_MAPPER_NAME, "plexos-usr");
    }
}
