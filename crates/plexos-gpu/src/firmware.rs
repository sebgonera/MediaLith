//! GuC and HuC firmware load status on Intel graphics.
//!
//! Intel's media pipeline depends on two firmware blobs the kernel loads at init. HuC
//! in particular is what gives QuickSync its quality at low bitrates, and when it fails
//! to load — missing `linux-firmware`, a kernel built without it, a BIOS that hides the
//! iGPU — **transcoding still works**. It is just slower and looks worse, with nothing
//! anywhere saying why. That is precisely the class of silent degradation PlexOS exists
//! to surface, so it is worth a module.
//!
//! The status lives in debugfs, which means two things this module takes seriously:
//! the paths have moved between kernel versions, and debugfs is frequently not mounted
//! or not readable. [`LoadState::Unknown`] is therefore a normal, non-alarming outcome
//! — "we could not tell" is reported as exactly that, never as a failure.

use serde::{Deserialize, Serialize};

use crate::env::Environment;
use crate::gpu::{Gpu, Vendor};

/// Whether a firmware blob is loaded and running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    /// Loaded and running.
    Running,
    /// Supported by the hardware but not running. Degrades transcoding silently.
    NotRunning,
    /// Not applicable to this hardware or driver.
    NotApplicable,
    /// Could not be determined, usually because debugfs is unmounted or unreadable.
    ///
    /// Not a problem in itself. Most systems report this.
    Unknown,
}

/// GuC and HuC status for one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareStatus {
    /// Graphics microcontroller firmware.
    pub guc: LoadState,
    /// Media microcontroller firmware. The one that matters for transcode quality.
    pub huc: LoadState,
}

impl FirmwareStatus {
    /// Nothing could be determined.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            guc: LoadState::Unknown,
            huc: LoadState::Unknown,
        }
    }

    /// Firmware this device supports is confirmed not running.
    ///
    /// Deliberately excludes [`LoadState::Unknown`]: an unreadable debugfs must never
    /// be reported to a user as broken firmware.
    #[must_use]
    pub fn has_confirmed_problem(self) -> bool {
        self.guc == LoadState::NotRunning || self.huc == LoadState::NotRunning
    }
}

/// debugfs locations that have carried this information, newest layout first.
///
/// The `gt0/` and `gt/` forms came with multi-tile support; the flat `i915_*_load_status`
/// files are the older layout. All are tried because PlexOS cannot assume a kernel
/// version when diagnosing a machine it did not build.
const GUC_PATHS: &[&str] = &[
    "/sys/kernel/debug/dri/0/gt0/uc/guc_info",
    "/sys/kernel/debug/dri/0/gt/uc/guc_info",
    "/sys/kernel/debug/dri/0/i915_guc_load_status",
];

const HUC_PATHS: &[&str] = &[
    "/sys/kernel/debug/dri/0/gt0/uc/huc_info",
    "/sys/kernel/debug/dri/0/gt/uc/huc_info",
    "/sys/kernel/debug/dri/0/i915_huc_load_status",
];

/// Interprets the contents of a GuC or HuC debugfs file.
///
/// Deliberately loose. The exact wording has changed across kernel releases, so this
/// looks for signals rather than matching a format, and returns [`LoadState::Unknown`]
/// when it recognises nothing. Guessing here would produce a confident wrong answer,
/// which is worse than no answer.
fn interpret(contents: &str) -> LoadState {
    let text = contents.to_ascii_lowercase();

    if text.contains("not supported") || text.contains("not present") {
        return LoadState::NotApplicable;
    }
    // The file says which blob it wanted and that it did not get it. This used to fall
    // through to Unknown, and Unknown was then reported as "debugfs is probably not
    // mounted" -- a guess, about a file that had just been read successfully, which hid
    // a real fault for as long as nobody moved the image to different hardware.
    if text.contains("status: missing") || text.contains("status: error") {
        return LoadState::NotRunning;
    }
    // Checked before the positive signals: "authenticated: no" contains "authenticated".
    if text.contains("authenticated: no")
        || text.contains("status: disabled")
        || text.contains("disabled")
        || text.contains("fw not running")
    {
        return LoadState::NotRunning;
    }
    if text.contains("running") || text.contains("authenticated: yes") {
        return LoadState::Running;
    }
    LoadState::Unknown
}

fn probe(env: &impl Environment, paths: &[&str]) -> LoadState {
    for path in paths {
        if let Ok(contents) = env.read(std::path::Path::new(path)) {
            let state = interpret(&contents);
            if state != LoadState::Unknown {
                return state;
            }
        }
    }
    LoadState::Unknown
}

/// Reads firmware status for a device.
///
/// Only Intel has GuC and HuC; every other vendor reports
/// [`LoadState::NotApplicable`] rather than being silently skipped, so a report always
/// says why a line is absent.
pub fn status(env: &impl Environment, gpu: &Gpu) -> FirmwareStatus {
    if gpu.vendor != Vendor::Intel {
        return FirmwareStatus {
            guc: LoadState::NotApplicable,
            huc: LoadState::NotApplicable,
        };
    }
    FirmwareStatus {
        guc: probe(env, GUC_PATHS),
        huc: probe(env, HUC_PATHS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Fixture;
    use crate::gpu::discover;

    fn intel(fixture: Fixture) -> Fixture {
        fixture.render_node("renderD128", "i915", 0x8086, 0x46d0)
    }

    fn only_gpu(fixture: &Fixture) -> Gpu {
        discover(fixture).into_iter().next().unwrap()
    }

    #[test]
    fn reads_the_modern_multi_gt_layout() {
        let fixture = intel(Fixture::new())
            .file(
                "/sys/kernel/debug/dri/0/gt0/uc/guc_info",
                "GuC firmware: i915/adlp_guc_70.bin\nstatus: RUNNING\n",
            )
            .file(
                "/sys/kernel/debug/dri/0/gt0/uc/huc_info",
                "HuC firmware: i915/tgl_huc.bin\nstatus: RUNNING\nHuC authenticated: yes\n",
            );

        let status = status(&fixture, &only_gpu(&fixture));
        assert_eq!(status.guc, LoadState::Running);
        assert_eq!(status.huc, LoadState::Running);
        assert!(!status.has_confirmed_problem());
    }

    #[test]
    fn reads_the_older_flat_layout() {
        let fixture = intel(Fixture::new()).file(
            "/sys/kernel/debug/dri/0/i915_huc_load_status",
            "HuC firmware: i915/kbl_huc.bin\nstatus: RUNNING\n",
        );
        assert_eq!(
            status(&fixture, &only_gpu(&fixture)).huc,
            LoadState::Running
        );
    }

    #[test]
    fn detects_huc_present_but_unauthenticated() {
        // The silent quality killer: transcoding works, output is worse, nothing warns.
        let fixture = intel(Fixture::new()).file(
            "/sys/kernel/debug/dri/0/gt0/uc/huc_info",
            "HuC firmware: i915/tgl_huc.bin\nHuC authenticated: no\n",
        );

        let status = status(&fixture, &only_gpu(&fixture));
        assert_eq!(status.huc, LoadState::NotRunning);
        assert!(status.has_confirmed_problem());
    }

    #[test]
    fn an_unreadable_debugfs_is_unknown_not_broken() {
        let fixture = intel(Fixture::new());
        let status = status(&fixture, &only_gpu(&fixture));

        assert_eq!(status, FirmwareStatus::unknown());
        assert!(
            !status.has_confirmed_problem(),
            "an unmounted debugfs must never be reported as a firmware fault"
        );
    }

    #[test]
    fn non_intel_hardware_is_marked_not_applicable() {
        let fixture = Fixture::new().render_node("renderD128", "amdgpu", 0x1002, 0x1636);
        let status = status(&fixture, &only_gpu(&fixture));

        assert_eq!(status.guc, LoadState::NotApplicable);
        assert_eq!(status.huc, LoadState::NotApplicable);
        assert!(!status.has_confirmed_problem());
    }

    #[test]
    fn falls_through_to_a_later_path_when_the_first_says_nothing_useful() {
        let fixture = intel(Fixture::new())
            .file("/sys/kernel/debug/dri/0/gt0/uc/guc_info", "\n")
            .file(
                "/sys/kernel/debug/dri/0/i915_guc_load_status",
                "status: RUNNING\n",
            );
        assert_eq!(
            status(&fixture, &only_gpu(&fixture)).guc,
            LoadState::Running
        );
    }

    #[test]
    fn unrecognised_wording_yields_unknown_rather_than_a_guess() {
        assert_eq!(
            interpret("some future format we have never seen"),
            LoadState::Unknown
        );
        assert_eq!(interpret(""), LoadState::Unknown);
        assert_eq!(
            interpret("HuC not supported by this platform"),
            LoadState::NotApplicable
        );
    }
}
