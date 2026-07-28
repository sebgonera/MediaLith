//! Bounding what Plex can consume, with cgroup v2.
//!
//! No syscalls: cgroup v2 is a filesystem, and every limit here is a small text file.
//! That is worth saying because it makes this the one part of ADR-0007's confinement
//! that can be read, changed and understood from a shell on the running machine.
//!
//! # What is limited, and what deliberately is not
//!
//! **Memory** and **process count** are bounded. A transcode that runs away takes the
//! machine's memory with it, and on an appliance with no swap the kernel's OOM killer
//! picks a victim that may not be Plex — the console, or `plexosd` holding the health
//! gate. Bounding Plex means the OOM killer's choice is made in advance and correctly.
//!
//! **CPU is not capped**, and that is a decision rather than an omission.
//! `CONFIG_CFS_BANDWIDTH` is unset in the kernel fragment, so `cpu.max` does not exist
//! — but even with it, capping the CPU of the one workload this appliance exists to
//! run would be throttling the product. Transcoding is supposed to use the machine.
//! `cpu.weight` is set instead, which changes nothing while Plex is alone and gives the
//! console a chance to answer while a transcode is saturating every core.
//!
//! # Limits are proportions, not numbers
//!
//! A fixed "2 GB" is wrong on both a 4 GB mini-PC and a 32 GB server. The memory bound
//! is a share of what the machine has, floored so that a small machine still gets a
//! usable Plex rather than one that cannot start.

use std::io;
use std::path::{Path, PathBuf};

/// Where cgroup v2 is mounted.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The cgroup Plex runs in.
pub const PLEX_CGROUP: &str = "plex";

/// Share of total memory Plex may use.
///
/// 80%: Plex is the reason the machine exists, so it gets nearly all of it, while the
/// remainder keeps `plexosd` and the console alive to report what happened when a
/// transcode misbehaves. A machine where the console dies alongside Plex is one nobody
/// can diagnose remotely.
const MEMORY_SHARE: u64 = 80;

/// Never bound memory below this, whatever the share works out to.
///
/// Plex will not start in much less, and a limit that prevents startup produces a
/// crash loop rather than a degraded service — the failure is louder and less useful.
const MEMORY_FLOOR: u64 = 1024 * 1024 * 1024;

/// Maximum processes and threads.
///
/// Plex spawns a transcoder per stream plus scanners; a few hundred is generous and a
/// fork bomb is thousands. This bounds the blast radius without bounding normal use.
const PIDS_MAX: u64 = 512;

/// Relative CPU share. 100 is the default weight; this is deliberately unchanged from
/// it for Plex and documented so that the *absence* of a cap is visible.
const CPU_WEIGHT: u64 = 100;

/// One file to write into a cgroup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limit {
    /// File name relative to the cgroup directory, e.g. `memory.max`.
    pub file: String,
    /// What to write.
    pub value: String,
}

/// The limits for a machine with `total_memory` bytes.
///
/// Pure, so the arithmetic can be checked against machines this project will never see.
#[must_use]
pub fn limits_for(total_memory: u64) -> Vec<Limit> {
    let share = total_memory / 100 * MEMORY_SHARE;
    let memory_max = share.max(MEMORY_FLOOR);

    vec![
        Limit {
            file: "memory.max".to_owned(),
            value: memory_max.to_string(),
        },
        Limit {
            file: "pids.max".to_owned(),
            value: PIDS_MAX.to_string(),
        },
        Limit {
            file: "cpu.weight".to_owned(),
            value: CPU_WEIGHT.to_string(),
        },
    ]
}

/// Total usable memory, from `MemTotal` in `/proc/meminfo`.
///
/// Returns `None` rather than a guess: a wrong total produces a wrong limit, and a
/// wrong limit either throttles Plex or fails to bound it. The caller decides what to
/// do with not knowing.
#[must_use]
pub fn total_memory(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    // The field is in kibibytes, which is the one detail that makes a limit a thousand
    // times too small if it is missed -- and 1 MB is enough to look plausible.
    Some(kib * 1024)
}

/// Which controllers must be delegated for the limits above to exist.
const REQUIRED_CONTROLLERS: [&str; 3] = ["memory", "pids", "cpu"];

/// Controllers named in a `cgroup.controllers` file that are missing from it.
///
/// A limit written to a cgroup whose controller was never enabled fails with `ENOENT`
/// on a file that plainly should be there, which reads as a kernel problem rather than
/// as a delegation that was not done.
#[must_use]
pub fn missing_controllers(available: &str) -> Vec<&'static str> {
    let present: Vec<&str> = available.split_whitespace().collect();
    REQUIRED_CONTROLLERS
        .into_iter()
        .filter(|wanted| !present.contains(wanted))
        .collect()
}

/// What to write into the parent's `cgroup.subtree_control` to delegate them.
#[must_use]
pub fn delegation() -> String {
    REQUIRED_CONTROLLERS
        .iter()
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Creates the cgroup and applies the limits.
///
/// A limit that cannot be written is logged and skipped rather than fatal. The
/// alternative is refusing to start Plex because `pids.max` was unavailable, which
/// trades a bounded service for no service — and the bound that matters most, memory,
/// is usually the one that does work.
///
/// # Errors
/// Only if the cgroup directory itself cannot be created, which means cgroup v2 is not
/// mounted and none of the limits could apply.
pub fn apply(root: &Path, total_memory: u64, log: &mut dyn FnMut(&str)) -> io::Result<PathBuf> {
    let group = root.join(PLEX_CGROUP);
    std::fs::create_dir_all(&group).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not create {}: {error}. cgroup v2 is probably not mounted at {}, \
                 so Plex would run unbounded.",
                group.display(),
                root.display()
            ),
        )
    })?;

    delegate(root, log);

    for limit in limits_for(total_memory) {
        let path = group.join(&limit.file);
        if let Err(error) = std::fs::write(&path, &limit.value) {
            log(&format!(
                "could not set {} to {}: {error}. Plex runs without that bound.",
                limit.file, limit.value
            ));
        }
    }

    Ok(group)
}

/// Hands the controllers down to child cgroups, so the limits have files to be written
/// to.
///
/// This is the step that makes [`delegation`] and [`missing_controllers`] mean something.
/// Without it a controller enabled in the root is *not* available in a child: cgroup v2
/// requires the parent to name it in `cgroup.subtree_control` first. The symptom is not
/// an error anywhere near the cause — `memory.max` simply does not exist, `apply` logs
/// that it could not be written, and Plex runs unbounded on a machine whose kernel
/// supports every bound asked for.
///
/// Reported and never fatal, for the reason [`apply`] gives: an unbounded Plex is worse
/// than a bounded one and far better than none.
fn delegate(root: &Path, log: &mut dyn FnMut(&str)) {
    let available = std::fs::read_to_string(root.join("cgroup.controllers")).unwrap_or_default();

    let missing = missing_controllers(&available);
    if !missing.is_empty() {
        log(&format!(
            "the kernel offers no {} controller at {}, so that bound cannot exist. \
             Check CONFIG_MEMCG, CONFIG_CGROUP_PIDS and CONFIG_CGROUP_SCHED in the \
             kernel configuration; this is an image fault, not a runtime one.",
            missing.join(", "),
            root.display()
        ));
    }

    let subtree = root.join("cgroup.subtree_control");
    if let Err(error) = std::fs::write(&subtree, delegation()) {
        log(&format!(
            "could not delegate controllers by writing {:?} to {}: {error}. The limits \
             below will have no files to be written to, and Plex runs unbounded.",
            delegation(),
            subtree.display()
        ));
    }
}

/// Moves a process into the cgroup.
///
/// Writing to `cgroup.procs` moves the whole thread group, so this is done to the child
/// before it execs Plex rather than to Plex afterwards — there is no window in which
/// the process exists outside the bound.
///
/// # Errors
/// If the write fails, which means the process is running unbounded and the caller
/// should say so rather than assume otherwise.
pub fn join(group: &Path, pid: u32) -> io::Result<()> {
    let procs = group.join("cgroup.procs");
    std::fs::write(&procs, pid.to_string()).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not move process {pid} into {}: {error}. It is running without \
                 the memory and process limits.",
                group.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cgroup root as the kernel presents one, in a directory a test can write to.
    fn mock_root(name: &str, controllers: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("cgroup.controllers"), controllers).unwrap();
        std::fs::write(root.join("cgroup.subtree_control"), "").unwrap();
        root
    }

    #[test]
    fn applying_limits_delegates_the_controllers_first() {
        // The step that was missing. cgroup v2 does not make a controller available in a
        // child merely because the root has it: the parent must name it in
        // cgroup.subtree_control. Without this, memory.max does not exist in the child,
        // apply() logs that it could not write it, and Plex runs unbounded on a machine
        // whose kernel supports every bound asked for.
        let root = mock_root("plexos-cgroup-delegate", "cpuset cpu io memory pids");
        let mut lines = Vec::new();
        let group = apply(&root, 8 * 1024 * 1024 * 1024, &mut |line| {
            lines.push(line.to_owned());
        })
        .unwrap();

        let written = std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap();
        for controller in REQUIRED_CONTROLLERS {
            assert!(
                written.contains(&format!("+{controller}")),
                "{controller} was never delegated: {written:?}"
            );
        }
        assert!(group.ends_with(PLEX_CGROUP));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_kernel_without_a_controller_is_named_an_image_fault() {
        // A missing controller and a failed delegation take opposite responses: one is
        // a kernel configuration to change and rebuild, the other is something wrong at
        // runtime. Reporting them the same way sends someone to the wrong place.
        let root = mock_root("plexos-cgroup-no-memory", "cpuset cpu io");
        let mut lines = Vec::new();
        apply(&root, 8 * 1024 * 1024 * 1024, &mut |line| {
            lines.push(line.to_owned());
        })
        .unwrap();

        let logged = lines.join("\n");
        assert!(logged.contains("memory"), "{logged}");
        assert!(
            logged.contains("CONFIG_MEMCG"),
            "and names the remedy: {logged}"
        );
        assert!(logged.contains("image fault"), "{logged}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delegation_that_cannot_be_written_is_reported_and_not_fatal() {
        // An unbounded Plex is worse than a bounded one and far better than none.
        let root = mock_root("plexos-cgroup-readonly", "memory pids cpu");
        std::fs::remove_file(root.join("cgroup.subtree_control")).unwrap();
        std::fs::create_dir(root.join("cgroup.subtree_control")).unwrap();

        let mut lines = Vec::new();
        let outcome = apply(&root, 8 * 1024 * 1024 * 1024, &mut |line| {
            lines.push(line.to_owned());
        });
        assert!(outcome.is_ok(), "the cgroup is still created");
        assert!(
            lines.join("\n").contains("runs unbounded"),
            "and says what the consequence is: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    fn memory_max(total: u64) -> u64 {
        limits_for(total)
            .into_iter()
            .find(|l| l.file == "memory.max")
            .expect("a memory limit")
            .value
            .parse()
            .expect("a number")
    }

    #[test]
    fn meminfo_is_read_as_kibibytes() {
        // The field is in kB. Reading it as bytes gives a limit a thousand times too
        // small, and 8 MB is plausible enough that nobody looks twice -- Plex simply
        // fails to start and the reason is a number in a file nobody reads.
        let meminfo = "MemTotal:        8127396 kB\nMemFree:  100 kB\n";
        assert_eq!(total_memory(meminfo), Some(8_127_396 * 1024));
    }

    #[test]
    fn an_unreadable_meminfo_yields_nothing_rather_than_a_guess() {
        // A wrong total is worse than no total: it produces a confident limit that is
        // wrong in a direction nobody checks.
        assert_eq!(total_memory(""), None);
        assert_eq!(total_memory("MemTotal: not-a-number kB"), None);
        assert_eq!(total_memory("MemFree: 100 kB"), None);
    }

    #[test]
    fn the_bound_is_a_share_on_a_large_machine() {
        assert_eq!(memory_max(32 * GIB), 32 * GIB / 100 * 80);
    }

    #[test]
    fn a_small_machine_gets_the_floor_rather_than_an_unstartable_plex() {
        // 80% of 512 MB is 410 MB, which Plex will not start in. A limit that prevents
        // startup gives a crash loop instead of a degraded service.
        let tiny = 512 * 1024 * 1024;
        assert_eq!(memory_max(tiny), MEMORY_FLOOR);
        assert!(memory_max(tiny) > tiny / 100 * MEMORY_SHARE);
    }

    #[test]
    fn something_is_always_left_for_the_console_on_a_normal_machine() {
        // If Plex could take all of it, the OOM killer would be choosing between Plex
        // and the daemon that reports on Plex -- and a machine whose console dies with
        // the workload is one nobody can diagnose from another room.
        for total in [4 * GIB, 8 * GIB, 16 * GIB, 32 * GIB] {
            assert!(
                memory_max(total) < total,
                "{total} would leave nothing for plexosd"
            );
        }
    }

    #[test]
    fn cpu_is_weighted_and_never_capped() {
        // CONFIG_CFS_BANDWIDTH is unset, so cpu.max does not exist -- and capping the
        // CPU of the workload this appliance exists to run would be throttling the
        // product. The absence is asserted so that adding one is a deliberate act.
        let files: Vec<String> = limits_for(8 * GIB).into_iter().map(|l| l.file).collect();
        assert!(files.contains(&"cpu.weight".to_owned()));
        assert!(!files.contains(&"cpu.max".to_owned()), "{files:?}");
    }

    #[test]
    fn every_controller_the_limits_need_is_one_we_delegate() {
        // A limit written into a cgroup whose controller was never enabled fails with
        // ENOENT on a file that obviously ought to exist, which reads as a kernel fault
        // rather than as missing delegation.
        for limit in limits_for(8 * GIB) {
            let controller = limit.file.split('.').next().unwrap().to_owned();
            assert!(
                REQUIRED_CONTROLLERS.contains(&controller.as_str()),
                "{} needs the {controller} controller, which is not delegated",
                limit.file
            );
        }
    }

    #[test]
    fn missing_controllers_are_named_individually() {
        assert_eq!(
            missing_controllers("memory pids cpu io"),
            Vec::<&str>::new()
        );
        assert_eq!(missing_controllers("memory cpu"), vec!["pids"]);
        assert_eq!(missing_controllers(""), vec!["memory", "pids", "cpu"]);
    }

    #[test]
    fn delegation_is_written_the_way_subtree_control_expects() {
        // Plus-prefixed and space-separated. Writing bare names silently enables
        // nothing.
        let written = delegation();
        assert_eq!(written, "+memory +pids +cpu");
        for controller in REQUIRED_CONTROLLERS {
            assert!(written.contains(&format!("+{controller}")));
        }
    }
}
