//! The health verdict.
//!
//! This is the module that justifies the crate. On a conventional distribution, a
//! broken hardware transcode path produces no message at all — Plex quietly falls back
//! to software, the CPU saturates, playback stutters, and the user is left guessing.
//!
//! So the contract here is: **every finding names a remedy**. A report that says
//! "hardware acceleration unavailable" and stops has reproduced the problem it was
//! written to fix. If this module cannot say what to do about a condition, it should
//! not be reporting that condition as a problem.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::env::Environment;
use crate::firmware::{self, FirmwareStatus, LoadState};
use crate::gpu::{self, Gpu};
use crate::vainfo::{self, Capabilities, Codec, OPTIONAL_DECODE, ProbeError};

/// Overall state of the hardware transcode path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Hardware transcoding works and is fully capable.
    Ready,
    /// Hardware transcoding works, but something will make it slower or worse.
    Degraded,
    /// Hardware transcoding is not available. Plex will use the CPU.
    Unavailable,
}

impl Health {
    /// Whether Plex can hardware transcode at all.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::Degraded)
    }
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Hardware transcoding cannot work until this is fixed.
    Critical,
    /// Hardware transcoding works but is degraded.
    Warning,
    /// Worth knowing, no action required.
    Info,
}

/// One observation, with what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// How much it matters.
    pub severity: Severity,
    /// What was observed.
    pub summary: String,
    /// What to do about it. Never empty for a warning or a critical finding.
    pub remedy: String,
}

impl Finding {
    fn new(severity: Severity, summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            severity,
            summary: summary.into(),
            remedy: remedy.into(),
        }
    }
}

/// The complete result of a self-test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Overall verdict.
    pub health: Health,
    /// Every graphics device found, including ones that cannot transcode.
    pub gpus: Vec<Gpu>,
    /// The device selected for transcoding, if any.
    pub primary: Option<Gpu>,
    /// Firmware status for the selected device.
    pub firmware: Option<FirmwareStatus>,
    /// VA-API capabilities of the selected device.
    pub capabilities: Option<Capabilities>,
    /// Observations, most severe first.
    pub findings: Vec<Finding>,
}

impl Report {
    /// Runs the full self-test.
    pub fn generate(env: &impl Environment) -> Self {
        let gpus = gpu::discover(env);
        let mut findings = Vec::new();

        let Some(primary) = gpu::select_primary(&gpus).cloned() else {
            findings.push(no_usable_gpu_finding(
                &gpus,
                &crate::gpu::display_devices(env),
            ));
            return Self {
                health: Health::Unavailable,
                gpus,
                primary: None,
                firmware: None,
                capabilities: None,
                findings,
            };
        };

        let firmware = firmware::status(env, &primary);
        findings.extend(firmware_findings(firmware));
        findings.extend(render_node_reachable_finding(env, &primary));

        let capabilities = match vainfo::probe(env, &primary) {
            Ok(caps) => caps,
            Err(error) => {
                findings.push(probe_failure_finding(&error));
                findings.sort_by_key(|f| f.severity);
                return Self {
                    health: Health::Unavailable,
                    gpus,
                    primary: Some(primary),
                    firmware: Some(firmware),
                    capabilities: None,
                    findings,
                };
            }
        };

        findings.extend(capability_findings(&capabilities));

        let health = if findings.iter().any(|f| f.severity == Severity::Critical) {
            Health::Unavailable
        } else if findings.iter().any(|f| f.severity == Severity::Warning) {
            Health::Degraded
        } else {
            Health::Ready
        };

        findings.sort_by_key(|f| f.severity);

        Self {
            health,
            gpus,
            primary: Some(primary),
            firmware: Some(firmware),
            capabilities: Some(capabilities),
            findings,
        }
    }

    /// Renders the report as JSON, for `plexosd` and the setup UI.
    ///
    /// # Errors
    /// Fails only if serialisation fails, which cannot happen for these types.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

/// Why there is no render node, told apart from what PCI actually shows.
///
/// The distinction this makes is the whole reason [`crate::gpu::display_devices`] exists.
/// A render node appears only once a kernel driver has bound, so through `/sys/class/drm`
/// a machine whose graphics card the kernel cannot drive is indistinguishable from a
/// machine with no graphics card — and the remedies are opposite.
///
/// This was found the way the trap list says these things are found: on a machine. The
/// previous version of this said "No graphics device found" and advised enabling the
/// integrated GPU in firmware, to somebody running a discrete RTX card in a system with
/// no integrated graphics at all. A remedy for the wrong machine sends a person to look
/// for a BIOS setting that does not exist.
fn nothing_to_render_with(present: &[crate::gpu::DisplayDevice]) -> Finding {
    let Some(device) = present.first() else {
        return Finding::new(
            Severity::Critical,
            "No graphics device found, on the PCI bus or anywhere else",
            "Check that the integrated GPU is enabled in firmware setup. Some BIOSes \
             disable it automatically when no display is attached — look for a setting \
             such as \"iGPU Multi-Monitor\" or \"Primary Display\" and force it on. \
             This report looked at the PCI bus as well, and there is genuinely no \
             display controller there.",
        );
    };

    if let Some(driver) = &device.kernel_driver {
        return Finding::new(
            Severity::Critical,
            format!(
                "{:?} device {:04x} at {} is driven by {driver}, but exposes no render node",
                device.vendor, device.device_id, device.slot
            ),
            "The kernel bound a driver and no `renderD*` node appeared, which is what \
             an unprivileged process needs. Usually this means the driver loaded in a \
             display-only mode because firmware it wanted was missing — check the boot \
             messages for that driver.",
        );
    }

    let remedy = match device.vendor {
        crate::gpu::Vendor::Nvidia => {
            "This image has no NVIDIA driver. Its kernel builds `i915` for Intel \
             graphics and nothing else, so nothing binds to this card and `/dev/dri` is \
             never created. NVIDIA is not a matter of enabling a kernel option: Plex \
             reaches NVDEC and NVENC through NVIDIA's own userspace libraries, which \
             need the matching kernel module — and for this generation that is the open \
             module from driver 570 or newer, which this image does not carry. Until \
             that exists, transcoding on this machine runs on the CPU. An Intel \
             integrated GPU, or a card the kernel can drive, is the supported path today."
        }
        crate::gpu::Vendor::Amd => {
            "This image has no AMD driver. Its kernel builds `i915` for Intel graphics \
             and nothing else, so nothing binds to this card. `amdgpu` and its firmware \
             would have to be added to the kernel and the initramfs before this card \
             could be used; until then, transcoding runs on the CPU."
        }
        crate::gpu::Vendor::Intel => {
            "An Intel device the kernel did not bind to, which is unexpected: this image \
             builds both `i915` and `xe`, which between them cover everything from \
             Broadwell to the current Arc parts. Check the boot messages for that \
             device — the likeliest cause is firmware the driver wanted and did not \
             find, since firmware for a built-in driver has to be in the initramfs."
        }
        crate::gpu::Vendor::Other(id) => {
            return Finding::new(
                Severity::Critical,
                format!(
                    "Display device {:04x}:{:04x} at {} has no kernel driver",
                    id, device.device_id, device.slot
                ),
                "Nothing in this image binds to it, so there is no render node and \
                 transcoding runs on the CPU. This kernel carries `i915` for Intel \
                 graphics only.",
            );
        }
    };

    Finding::new(
        Severity::Critical,
        format!(
            "{:?} device {:04x} at {} has no kernel driver bound",
            device.vendor, device.device_id, device.slot
        ),
        remedy,
    )
}

/// Whether the account Plex runs as can open the render node at all.
///
/// Everything else in this report is a root process asking the hardware what it can do,
/// and the answer has been `ready` on a machine where Plex was transcoding on the CPU.
/// That is the failure this whole crate exists to prevent, arrived at from a direction
/// nobody had covered: the capability was real, the driver was right, the Landlock grant
/// was correct — and the device node was `0600 root:root`, because DRM does not set a
/// mode and there is no `udev` here to relax it.
///
/// Landlock cannot paper over this. It only ever restricts what the ordinary permissions
/// already allow, so a grant on `/dev/dri` looks correct and grants nothing.
fn render_node_reachable_finding(env: &impl Environment, primary: &Gpu) -> Option<Finding> {
    // Absent is not a failure here: a missing node is reported elsewhere in its own
    // words, and inventing a second message for it would be two answers to one question.
    let mode = env.mode(&primary.render_node)?;

    // The `other` bits. Plex runs as its own uid with supplementary groups deliberately
    // cleared, so group ownership cannot help it either -- world-accessible is the only
    // thing that works, which is exactly what every distribution's udev rule sets.
    if mode & 0o006 != 0 {
        return None;
    }

    Some(Finding::new(
        Severity::Critical,
        format!(
            "{} is mode {:04o} and Plex cannot open it",
            primary.render_node.display(),
            mode & 0o777
        ),
        "Everything above this line is true and Plex will still transcode on the CPU: \
         these capabilities were probed as root, and Plex runs unprivileged. DRM leaves \
         render nodes at 0600 and every ordinary distribution relaxes them with a udev \
         rule; PlexOS has no udev, so plexosd does it before starting Plex. Seeing this \
         means that step did not run or did not work — check the boot log for the line \
         about the render node.",
    ))
}

fn no_usable_gpu_finding(gpus: &[Gpu], present: &[crate::gpu::DisplayDevice]) -> Finding {
    if gpus.is_empty() {
        return nothing_to_render_with(present);
    }
    let vendors: Vec<String> = gpus
        .iter()
        .map(|g| format!("{:?} ({})", g.vendor, g.kernel_driver))
        .collect();
    Finding::new(
        Severity::Critical,
        format!(
            "Graphics devices found, but none supported for hardware transcoding: {}",
            vendors.join(", ")
        ),
        "PlexOS v1 supports Intel iGPUs (QuickSync) and AMD via VA-API. NVENC is not \
         supported in this release.",
    )
}

fn firmware_findings(status: FirmwareStatus) -> Vec<Finding> {
    let mut findings = Vec::new();

    if status.huc == LoadState::NotRunning {
        findings.push(Finding::new(
            Severity::Warning,
            "Intel HuC firmware is not running",
            "Transcoding will work but produce noticeably worse quality at a given \
             bitrate. Install the linux-firmware package containing the HuC blob for \
             this GPU and reboot.",
        ));
    }
    if status.guc == LoadState::NotRunning {
        findings.push(Finding::new(
            Severity::Warning,
            "Intel GuC firmware is not running",
            "Install the linux-firmware package containing the GuC blob for this GPU \
             and reboot. HuC cannot be authenticated without GuC.",
        ));
    }
    // Unknown is the common case off the appliance: debugfs is usually unmounted or
    // unreadable. It is reported so the absence is visible.
    //
    // The remedy used to end "the transcode test below is the authoritative check".
    // There is no transcode test, here or anywhere in this crate, and there never has
    // been. Pointing a reader at a check that does not exist is the trap already in
    // CLAUDE.md about `could not bind :80`: a wrong remedy costs more than none,
    // because it is followed. On PlexOS this state now means something has gone wrong
    // with the mount, since plexos-init mounts debugfs before anything reads this.
    if status.huc == LoadState::Unknown && status.guc == LoadState::Unknown {
        findings.push(Finding::new(
            Severity::Info,
            "Could not determine GuC/HuC firmware status",
            "The state lives in debugfs, and debugfs is not mounted. On PlexOS \
             plexos-init mounts it during startup, so this means that mount failed — \
             check the boot log. Elsewhere, `mount -t debugfs debugfs /sys/kernel/debug` \
             makes it readable. HuC affects transcode quality at low bitrates, so this \
             is worth resolving rather than living with.",
        ));
    }
    findings
}

fn probe_failure_finding(error: &ProbeError) -> Finding {
    let remedy = match error {
        ProbeError::ToolMissing => {
            "Install vainfo (the libva-utils package). PlexOS images include it; this \
             usually means the probe is running on a system that is not PlexOS."
        }
        ProbeError::DriverFailed(_) => {
            "The VA-API driver could not be loaded. On Intel, install the \
             intel-media-driver package providing iHD_drv_video.so, and confirm the \
             render node is readable by the Plex user."
        }
        ProbeError::NoProfiles => {
            "The driver loaded but exposed no codecs, which usually means it matched \
             the wrong hardware generation. Check that iHD rather than i965 was \
             selected for this GPU."
        }
    };
    Finding::new(Severity::Critical, error.to_string(), remedy)
}

fn capability_findings(caps: &Capabilities) -> Vec<Finding> {
    let mut findings = Vec::new();

    if !caps.can_encode(Codec::H264) {
        findings.push(Finding::new(
            Severity::Critical,
            "No hardware H.264 encoder",
            "Plex transcodes to H.264 for almost every client, so without this there \
             is no hardware transcoding to speak of. Confirm the GPU is not a \
             display-only or virtualised device.",
        ));
    }

    let missing = caps.missing_required_decode();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|c| c.name()).collect();
        findings.push(Finding::new(
            Severity::Warning,
            format!("No hardware decoder for: {}", names.join(", ")),
            "These sources will be decoded on the CPU, which costs far more than \
             encoding does. Usually means an older GPU generation than the media \
             requires — HEVC 10-bit in particular needs Kaby Lake or newer.",
        ));
    }

    let absent_optional: Vec<&str> = OPTIONAL_DECODE
        .iter()
        .filter(|c| !caps.can_decode(**c))
        .map(|c| c.name())
        .collect();
    if !absent_optional.is_empty() {
        findings.push(Finding::new(
            Severity::Info,
            format!("No hardware decoder for: {}", absent_optional.join(", ")),
            "Not required. These are decoded on the CPU, which is acceptable for the \
             small share of libraries that use them.",
        ));
    }

    findings
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verdict = match self.health {
            Health::Ready => "READY      hardware transcoding is available",
            Health::Degraded => "DEGRADED   hardware transcoding works, with caveats",
            Health::Unavailable => "UNAVAILABLE  Plex will transcode on the CPU",
        };
        writeln!(f, "{verdict}")?;

        if let Some(gpu) = &self.primary {
            writeln!(f)?;
            writeln!(
                f,
                "  Device    {} [{:04x}] via {}",
                gpu.model.as_deref().unwrap_or("unrecognised model"),
                gpu.device_id,
                gpu.kernel_driver
            )?;
            writeln!(f, "  Node      {}", gpu.render_node.display())?;
        }
        if let Some(caps) = &self.capabilities {
            if let Some(driver) = &caps.driver {
                writeln!(f, "  Driver    {driver}")?;
            }
            let decode: Vec<&str> = [Codec::H264, Codec::Hevc, Codec::Hevc10, Codec::Av1]
                .iter()
                .filter(|c| caps.can_decode(**c))
                .map(|c| c.name())
                .collect();
            writeln!(f, "  Decode    {}", join_or_none(&decode))?;
            let encode: Vec<&str> = [Codec::H264, Codec::Hevc]
                .iter()
                .filter(|c| caps.can_encode(**c))
                .map(|c| c.name())
                .collect();
            writeln!(f, "  Encode    {}", join_or_none(&encode))?;
        }

        if !self.findings.is_empty() {
            writeln!(f)?;
            for finding in &self.findings {
                let tag = match finding.severity {
                    Severity::Critical => "CRITICAL",
                    Severity::Warning => "WARNING ",
                    Severity::Info => "INFO    ",
                };
                writeln!(f, "  {tag}  {}", finding.summary)?;
                writeln!(f, "            {}", finding.remedy)?;
            }
        }
        Ok(())
    }
}

fn join_or_none(items: &[&str]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Fixture;

    const WORKING_VAINFO: &str = include_str!("../tests/fixtures/vainfo-adl-n-ihd.txt");

    fn healthy_machine() -> Fixture {
        Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x46d0)
            .command("vainfo", WORKING_VAINFO)
            .file(
                "/sys/kernel/debug/dri/0/gt0/uc/guc_info",
                "status: RUNNING\n",
            )
            .file(
                "/sys/kernel/debug/dri/0/gt0/uc/huc_info",
                "HuC authenticated: yes\n",
            )
    }

    #[test]
    fn a_root_only_render_node_is_reported_however_capable_the_hardware_is() {
        // The failure that produced this test: a laptop whose GPU reported `ready` with
        // fifty-five VA-API entries while Plex transcoded on the CPU. Every probe here
        // runs as root; Plex does not. DRM leaves render nodes at 0600 and there is no
        // udev in this image to relax them.
        let gpu = Gpu {
            node: "renderD128".to_owned(),
            render_node: std::path::PathBuf::from("/dev/dri/renderD128"),
            vendor: crate::gpu::Vendor::Intel,
            device_id: 0x46b3,
            kernel_driver: "i915".to_owned(),
            model: None,
        };
        let env = Fixture::new()
            .file("/dev/dri/renderD128", String::new())
            .mode("/dev/dri/renderD128", 0o600);

        let finding = render_node_reachable_finding(&env, &gpu).expect("must be reported");
        assert_eq!(finding.severity, Severity::Critical);
        assert!(finding.summary.contains("0600"), "{finding:?}");
        assert!(
            finding.remedy.contains("probed as root"),
            "and says why everything above it looked fine: {finding:?}"
        );
    }

    #[test]
    fn a_world_accessible_render_node_says_nothing() {
        // What every machine with a udev rule looks like, and what plexosd now produces.
        let gpu = Gpu {
            node: "renderD128".to_owned(),
            render_node: std::path::PathBuf::from("/dev/dri/renderD128"),
            vendor: crate::gpu::Vendor::Intel,
            device_id: 0x46b3,
            kernel_driver: "i915".to_owned(),
            model: None,
        };
        let env = Fixture::new()
            .file("/dev/dri/renderD128", String::new())
            .mode("/dev/dri/renderD128", 0o666);

        assert!(render_node_reachable_finding(&env, &gpu).is_none());
    }

    #[test]
    fn a_card_with_no_driver_is_not_reported_as_no_card() {
        // Found on hardware: PlexOS was moved to a machine with an RTX card and no
        // integrated graphics. No kernel driver bound, so no render node, so the report
        // said "No graphics device found" and advised enabling the integrated GPU in
        // firmware -- on a machine that has none. A remedy for the wrong machine sends
        // somebody looking for a BIOS setting that does not exist.
        let present = [crate::gpu::DisplayDevice {
            slot: "0000:01:00.0".to_owned(),
            vendor: crate::gpu::Vendor::Nvidia,
            device_id: 0x2d05,
            kernel_driver: None,
        }];

        let finding = nothing_to_render_with(&present);
        assert!(finding.summary.contains("0000:01:00.0"), "{finding:?}");
        assert!(finding.summary.contains("no kernel driver"), "{finding:?}");
        assert!(
            !finding.remedy.contains("iGPU Multi-Monitor"),
            "the firmware advice must not be given to a machine with a discrete card"
        );
        assert!(finding.remedy.contains("NVIDIA"), "{finding:?}");
    }

    #[test]
    fn an_empty_pci_bus_still_gets_the_firmware_advice() {
        // The case the original message was written for, and it is still right there:
        // an integrated GPU switched off in firmware really does vanish from PCI.
        let finding = nothing_to_render_with(&[]);
        assert!(finding.remedy.contains("iGPU Multi-Monitor"), "{finding:?}");
        assert!(
            finding.remedy.contains("PCI bus"),
            "and says it checked, so the reader knows the difference was considered"
        );
    }

    #[test]
    fn a_bound_driver_with_no_render_node_is_a_third_thing() {
        // Distinct from both: the driver loaded and produced nothing usable, which is
        // what a missing firmware blob looks like.
        let present = [crate::gpu::DisplayDevice {
            slot: "0000:00:02.0".to_owned(),
            vendor: crate::gpu::Vendor::Intel,
            device_id: 0x3ea0,
            kernel_driver: Some("i915".to_owned()),
        }];

        let finding = nothing_to_render_with(&present);
        assert!(finding.summary.contains("i915"), "{finding:?}");
        assert!(finding.remedy.contains("firmware"), "{finding:?}");
    }

    #[test]
    fn a_healthy_n100_reports_ready_with_nothing_to_do() {
        let report = Report::generate(&healthy_machine());

        assert_eq!(report.health, Health::Ready);
        assert!(report.health.is_usable());
        assert!(
            report.findings.is_empty(),
            "a fully working machine should have nothing to report: {:?}",
            report.findings
        );
        assert_eq!(report.primary.unwrap().device_id, 0x46d0);
    }

    #[test]
    fn every_actionable_finding_carries_a_remedy() {
        // The whole point of the crate. A finding without a remedy reproduces the
        // silent-failure problem it exists to solve.
        let machines = [
            Fixture::new(),
            Fixture::new().render_node("renderD128", "nvidia", 0x10de, 0x1e84),
            Fixture::new().render_node("renderD128", "i915", 0x8086, 0x46d0),
            healthy_machine(),
        ];
        for machine in machines {
            for finding in Report::generate(&machine).findings {
                assert!(
                    !finding.remedy.trim().is_empty(),
                    "finding without a remedy: {}",
                    finding.summary
                );
            }
        }
    }

    #[test]
    fn a_machine_with_no_gpu_says_to_check_the_firmware_setup() {
        let report = Report::generate(&Fixture::new());

        assert_eq!(report.health, Health::Unavailable);
        assert!(!report.health.is_usable());
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].severity, Severity::Critical);
        assert!(report.findings[0].remedy.contains("iGPU Multi-Monitor"));
    }

    #[test]
    fn nvidia_only_is_reported_as_out_of_scope_not_as_broken_hardware() {
        let fixture = Fixture::new().render_node("renderD128", "nvidia", 0x10de, 0x1e84);
        let report = Report::generate(&fixture);

        assert_eq!(report.health, Health::Unavailable);
        assert!(report.primary.is_none());
        assert_eq!(report.gpus.len(), 1, "the device is still reported");
        assert!(report.findings[0].remedy.contains("NVENC"));
    }

    #[test]
    fn unauthenticated_huc_degrades_rather_than_disables() {
        let fixture = healthy_machine().file(
            "/sys/kernel/debug/dri/0/gt0/uc/huc_info",
            "HuC authenticated: no\n",
        );
        let report = Report::generate(&fixture);

        assert_eq!(report.health, Health::Degraded);
        assert!(report.health.is_usable(), "transcoding still works");
        let huc = report
            .findings
            .iter()
            .find(|f| f.summary.contains("HuC"))
            .expect("HuC finding");
        assert_eq!(huc.severity, Severity::Warning);
        assert!(huc.remedy.contains("linux-firmware"));
    }

    #[test]
    fn an_unreadable_debugfs_does_not_degrade_a_working_machine() {
        let fixture = Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x46d0)
            .command("vainfo", WORKING_VAINFO);
        let report = Report::generate(&fixture);

        assert_eq!(report.health, Health::Ready);
        assert!(
            report.findings.iter().all(|f| f.severity == Severity::Info),
            "unknown firmware status must not be actionable"
        );
    }

    #[test]
    fn a_driver_that_fails_to_load_is_critical_and_names_the_package() {
        let fixture = Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x46d0)
            .command(
                "vainfo",
                "libva info: Trying to open /usr/lib/dri/iHD_drv_video.so\n\
                 libva info: va_openDriver() returns -1\n",
            );
        let report = Report::generate(&fixture);

        assert_eq!(report.health, Health::Unavailable);
        assert!(report.capabilities.is_none());
        let critical = &report.findings[0];
        assert_eq!(critical.severity, Severity::Critical);
        assert!(critical.remedy.contains("intel-media-driver"));
    }

    #[test]
    fn missing_hevc_decode_degrades_but_h264_encode_keeps_it_usable() {
        let fixture = Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x0416)
            .command(
                "vainfo",
                "vainfo: Supported profile and entrypoints\n\
                 VAProfileH264High : VAEntrypointVLD\n\
                 VAProfileH264High : VAEntrypointEncSlice\n",
            );
        let report = Report::generate(&fixture);

        assert_eq!(report.health, Health::Degraded);
        let warning = report
            .findings
            .iter()
            .find(|f| f.severity == Severity::Warning)
            .expect("decode warning");
        assert!(warning.summary.contains("HEVC"));
    }

    #[test]
    fn findings_are_ordered_most_severe_first() {
        let fixture = Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x46d0)
            .file(
                "/sys/kernel/debug/dri/0/gt0/uc/huc_info",
                "HuC authenticated: no\n",
            )
            .command("vainfo", "libva info: va_openDriver() returns -1\n");
        let report = Report::generate(&fixture);

        let severities: Vec<Severity> = report.findings.iter().map(|f| f.severity).collect();
        let mut sorted = severities.clone();
        sorted.sort_unstable();
        assert_eq!(severities, sorted);
        assert_eq!(severities[0], Severity::Critical);
    }

    #[test]
    fn serialises_to_json_for_plexosd() {
        let json = Report::generate(&healthy_machine()).to_json().unwrap();
        let parsed: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.health, Health::Ready);
    }

    #[test]
    fn human_output_leads_with_the_verdict() {
        let text = Report::generate(&healthy_machine()).to_string();
        assert!(text.starts_with("READY"), "got: {text}");
        assert!(text.contains("Alder Lake-N"));
        assert!(text.contains("HEVC 10-bit"));
    }
}
