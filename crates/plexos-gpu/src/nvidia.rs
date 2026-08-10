//! The NVIDIA branch of the report, which ADR-0015 said would be needed.
//!
//! Everything else here asks VA-API. NVIDIA does not provide it and never will: decode
//! goes through NVDEC and encode through NVENC, reached by libraries rather than by
//! `/dev/dri/renderD*`. So a report built entirely around `vainfo` looked at a working
//! card, found no VA-API, and said — at `critical`, the strongest thing it can say —
//! that hardware transcoding was unsupported on a machine that was doing it.
//!
//! That is the trap this project already recorded once, in the boot gate: **a placeholder
//! that was correct once becomes a lie later.** "NVENC is not supported in this release"
//! was true when it was written and stayed in the binary after it stopped being true.
//!
//! # What is asked instead
//!
//! Three questions, in the order that tells them apart:
//!
//! 1. **Is the driver loaded?** `/proc/driver/nvidia/version` exists only if it is.
//! 2. **Are the device nodes there, and reachable by the account Plex runs as?** There is
//!    no `udev` here, so they are made by `plexos-init` and their mode is whatever made
//!    them. A node at `0600 root:root` is the render-node defect again: every probe run
//!    as root reports success while Plex quietly uses the CPU.
//! 3. **Does the GPU actually initialise?** `nvidia-smi` is the only thing that answers
//!    it, because the card is initialised lazily — the modules can be loaded, the nodes
//!    correct, and the GPU still asleep until a client opens it.
//!
//! Each failure has a different remedy, so each is reported separately rather than as one
//! "NVIDIA does not work".

use crate::env::Environment;
use crate::gpu::{Gpu, Vendor};
use crate::report::{Finding, Severity};
use std::path::Path;

/// Where the driver reports itself. Absent unless a module is loaded.
pub const VERSION_FILE: &str = "/proc/driver/nvidia/version";

/// The nodes libcuda opens, and which `plexos-init` creates because `devtmpfs` will not.
pub const REQUIRED_NODES: [&str; 3] = ["/dev/nvidiactl", "/dev/nvidia0", "/dev/nvidia-uvm"];

/// What the machine says about its NVIDIA stack.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// The driver version, if a module is loaded.
    pub driver: Option<String>,
    /// Each required node, and whether the Plex account could open it.
    pub nodes: Vec<(String, NodeState)>,
    /// What `nvidia-smi` reported, if it ran.
    pub model: Option<String>,
}

/// What is wrong with a device node, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    /// Present and reachable by an ordinary account.
    Usable,
    /// Present, but only root may use it.
    RootOnly,
    /// Not there at all.
    Missing,
}

/// Whether this machine has an NVIDIA card at all.
#[must_use]
pub fn card(gpus: &[Gpu]) -> Option<&Gpu> {
    gpus.iter().find(|gpu| gpu.vendor == Vendor::Nvidia)
}

/// Asks the three questions.
#[must_use]
pub fn status(env: &impl Environment) -> Status {
    let driver = env.read(Path::new(VERSION_FILE)).ok().and_then(|text| {
        text.lines()
            .next()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
    });

    let nodes = REQUIRED_NODES
        .iter()
        .map(|path| {
            let state = match env.mode(Path::new(path)) {
                // The other-bits are the whole question. Plex runs as uid 900, so a node
                // it cannot read and write is a node that is not there as far as
                // transcoding is concerned.
                Some(mode) if mode & 0o006 == 0o006 => NodeState::Usable,
                Some(_) => NodeState::RootOnly,
                None => NodeState::Missing,
            };
            ((*path).to_owned(), state)
        })
        .collect();

    // Only worth running if the driver is there; otherwise it fails for a reason already
    // being reported and adds a second message about the same fault.
    let model = driver.as_ref().and_then(|_| {
        env.run(
            "/usr/bin/nvidia-smi",
            &["--query-gpu=name", "--format=csv,noheader"],
        )
        .ok()
        .map(|out| out.trim().to_owned())
        .filter(|out| !out.is_empty())
    });

    Status {
        driver,
        nodes,
        model,
    }
}

/// Whether the stack is complete enough for Plex to use the card.
#[must_use]
pub fn usable(status: &Status) -> bool {
    status.driver.is_some()
        && status.model.is_some()
        && status
            .nodes
            .iter()
            .all(|(_, state)| *state == NodeState::Usable)
}

/// What is wrong, and what to do about each thing.
#[must_use]
pub fn findings(status: &Status) -> Vec<Finding> {
    let mut findings = Vec::new();

    let Some(driver) = &status.driver else {
        findings.push(Finding::new(
            Severity::Critical,
            "an NVIDIA card is present and its driver is not loaded",
            "plexos-init loads the modules at boot when it finds an NVIDIA card on the \
             PCI bus. Nothing here creates /proc/driver/nvidia, so its absence means the \
             load failed rather than that it was skipped -- read the boot log for a line \
             beginning `nvidia:`. A refused signature says `Key was rejected by service` \
             and means the modules were built against a different kernel.",
        ));
        return findings;
    };

    for (path, state) in &status.nodes {
        match state {
            NodeState::Usable => {}
            NodeState::Missing => findings.push(Finding::new(
                Severity::Critical,
                format!("{path} does not exist"),
                "There is no udev here and this driver does not register through the \
                 device model, so devtmpfs creates nothing: plexos-init makes these nodes \
                 from the majors in /proc/devices. A missing one means that step did not \
                 run or could not.",
            )),
            NodeState::RootOnly => findings.push(Finding::new(
                Severity::Critical,
                format!("{path} is reachable only by root"),
                "Plex runs as uid 900, so it cannot open this and will transcode on the \
                 CPU without reporting anything. Every probe above it runs as root and \
                 will keep saying the card is fine. The node needs mode 0666; note that \
                 mknod masks its mode with the umask, so asking for 0666 is not enough on \
                 its own.",
            )),
        }
    }

    if status.model.is_none() {
        findings.push(Finding::new(
            Severity::Critical,
            format!("the driver is loaded ({driver}) and the GPU did not initialise"),
            "nvidia-smi answers only once the card has come up, and it comes up lazily -- \
             the modules can be loaded and the nodes correct while the GPU is still \
             asleep. The usual cause is missing GSP firmware: the open modules do not run \
             without nvidia/<version>/gsp_ga10x.bin, which the plexos-nvidia package \
             installs into /usr/lib/firmware.",
        ));
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Fixture;

    /// The first line of /proc/driver/nvidia/version on the RTX 5060, captured rather
    /// than invented.
    const REAL_VERSION: &str = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  \
                                610.57.04  Release Build\n";

    fn working() -> Status {
        Status {
            driver: Some("NVRM version: 610.57.04".to_owned()),
            nodes: REQUIRED_NODES
                .iter()
                .map(|p| ((*p).to_owned(), NodeState::Usable))
                .collect(),
            model: Some("NVIDIA GeForce RTX 5060".to_owned()),
        }
    }

    #[test]
    fn a_working_stack_produces_no_findings_at_all() {
        // The whole point. This machine transcodes on the card, and the report used to
        // call that critical.
        assert!(usable(&working()));
        assert!(findings(&working()).is_empty());
    }

    #[test]
    fn a_root_only_node_is_critical_and_says_so_about_uid_900() {
        // The render-node defect, which was invisible precisely because everything above
        // it runs as root and reports success.
        let mut status = working();
        status.nodes[0].1 = NodeState::RootOnly;
        let found = findings(&status);
        assert_eq!(found[0].severity, Severity::Critical);
        assert!(
            found[0].remedy.contains("900"),
            "the remedy must name the account that cannot open it: {}",
            found[0].remedy
        );
        assert!(!usable(&status));
    }

    #[test]
    fn a_loaded_driver_with_a_sleeping_gpu_points_at_the_firmware() {
        // Modules loaded, nodes right, and nvidia-smi silent. That is GSP firmware, and
        // naming it is the difference between a remedy and a shrug.
        let mut status = working();
        status.model = None;
        let found = findings(&status);
        assert_eq!(found.len(), 1);
        assert!(found[0].remedy.contains("gsp_ga10x.bin"));
    }

    #[test]
    fn no_driver_reports_that_and_stops() {
        // One fault, one message. Listing three missing nodes as well would bury the
        // thing that caused them.
        let status = Status::default();
        let found = findings(&status);
        assert_eq!(found.len(), 1, "got {found:#?}");
        assert!(found[0].summary.contains("driver is not loaded"));
    }

    #[test]
    fn the_driver_line_is_read_from_where_the_kernel_writes_it() {
        let env = Fixture::new().file(VERSION_FILE, REAL_VERSION);
        let status = status(&env);
        assert!(
            status.driver.is_some_and(|d| d.contains("610.57.04")),
            "the version has to come out of /proc, not from a constant"
        );
    }

    #[test]
    fn a_node_readable_only_by_root_is_not_mistaken_for_a_working_one() {
        let env = Fixture::new()
            .file(VERSION_FILE, REAL_VERSION)
            .mode("/dev/nvidiactl", 0o600)
            .mode("/dev/nvidia0", 0o666)
            .mode("/dev/nvidia-uvm", 0o666);
        let status = status(&env);
        assert_eq!(status.nodes[0].1, NodeState::RootOnly);
        assert_eq!(status.nodes[1].1, NodeState::Usable);
    }
}
