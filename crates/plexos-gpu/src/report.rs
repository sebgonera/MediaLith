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
            findings.push(no_usable_gpu_finding(&gpus));
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

fn no_usable_gpu_finding(gpus: &[Gpu]) -> Finding {
    if gpus.is_empty() {
        return Finding::new(
            Severity::Critical,
            "No graphics device found",
            "Check that the integrated GPU is enabled in firmware setup. Some BIOSes \
             disable it automatically when no display is attached — look for a setting \
             such as \"iGPU Multi-Monitor\" or \"Primary Display\" and force it on.",
        );
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
    // Unknown is the common case: debugfs is usually unmounted or unreadable. It is
    // reported so the absence is visible, never as something to act on.
    if status.huc == LoadState::Unknown && status.guc == LoadState::Unknown {
        findings.push(Finding::new(
            Severity::Info,
            "Could not determine GuC/HuC firmware status",
            "This is normal when debugfs is not mounted. Firmware may well be loaded; \
             the transcode test below is the authoritative check.",
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
