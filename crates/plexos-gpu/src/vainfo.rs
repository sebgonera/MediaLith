//! Probing VA-API capabilities via `vainfo`.
//!
//! `vainfo` is the ground truth for what the stack can actually do, as opposed to what
//! the hardware theoretically supports. It exercises the same driver load path FFmpeg
//! and Plex use, so if `vainfo` cannot open the driver, neither can they — and its
//! failure text usually names the reason.
//!
//! Parsing is intentionally tolerant. `vainfo` interleaves `libva info:` chatter,
//! `XDG_RUNTIME_DIR` warnings, and driver banners with the table we want, and the exact
//! mix varies by version and environment. This module looks for the profile/entrypoint
//! lines and ignores everything it does not recognise.

use serde::{Deserialize, Serialize};

use crate::env::Environment;
use crate::gpu::{Gpu, VaapiDriver};

/// A codec, in the terms Plex cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// H.264 / AVC.
    H264,
    /// HEVC 8-bit.
    Hevc,
    /// HEVC 10-bit, which is most 4K content.
    Hevc10,
    /// AV1.
    Av1,
    /// VP9.
    Vp9,
}

impl Codec {
    /// The VA profile substring identifying this codec.
    const fn profile_marker(self) -> &'static str {
        match self {
            Self::H264 => "VAProfileH264",
            // Ordering matters at the call site: "VAProfileHEVCMain" is a prefix of
            // "VAProfileHEVCMain10", so 10-bit is matched by its own longer marker and
            // 8-bit excludes it explicitly. See `Capabilities::matches`.
            Self::Hevc => "VAProfileHEVCMain",
            Self::Hevc10 => "VAProfileHEVCMain10",
            Self::Av1 => "VAProfileAV1",
            Self::Vp9 => "VAProfileVP9",
        }
    }

    /// Human-readable name for reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC 8-bit",
            Self::Hevc10 => "HEVC 10-bit",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
        }
    }
}

/// What Plex needs before hardware transcoding is worth calling working.
///
/// Decode of the common delivery codecs, and H.264 encode because that is what Plex
/// transcodes *to* for nearly every client.
pub const REQUIRED_DECODE: &[Codec] = &[Codec::H264, Codec::Hevc, Codec::Hevc10];

/// Codecs whose absence is worth mentioning but is not a failure.
pub const OPTIONAL_DECODE: &[Codec] = &[Codec::Av1, Codec::Vp9];

/// One profile/entrypoint pair reported by `vainfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// e.g. `VAProfileH264High`.
    pub profile: String,
    /// e.g. `VAEntrypointVLD`.
    pub entrypoint: String,
}

impl Entry {
    /// Whether this entry represents decoding.
    #[must_use]
    pub fn is_decode(&self) -> bool {
        self.entrypoint.contains("VLD")
    }

    /// Whether this entry represents encoding.
    ///
    /// Covers both `EncSlice` and the low-power `EncSliceLP` path, which is what
    /// modern Intel hardware actually exposes for H.264.
    #[must_use]
    pub fn is_encode(&self) -> bool {
        self.entrypoint.contains("EncSlice") || self.entrypoint.contains("EncPicture")
    }
}

/// What the VA-API stack on this machine reports it can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// VA-API version string, when reported.
    pub api_version: Option<String>,
    /// Driver banner, e.g. the `iHD` version. Invaluable in a bug report.
    pub driver: Option<String>,
    /// Every profile/entrypoint pair reported.
    pub entries: Vec<Entry>,
}

impl Capabilities {
    fn matches(&self, codec: Codec) -> impl Iterator<Item = &Entry> {
        let marker = codec.profile_marker();
        // HEVC 8-bit and 10-bit share a prefix. Without this exclusion a machine that
        // decodes only 10-bit would be reported as decoding 8-bit too.
        let exclude_10bit = codec == Codec::Hevc;
        self.entries.iter().filter(move |e| {
            e.profile.contains(marker) && !(exclude_10bit && e.profile.contains("Main10"))
        })
    }

    /// Whether the stack can decode this codec in hardware.
    #[must_use]
    pub fn can_decode(&self, codec: Codec) -> bool {
        self.matches(codec).any(Entry::is_decode)
    }

    /// Whether the stack can encode this codec in hardware.
    #[must_use]
    pub fn can_encode(&self, codec: Codec) -> bool {
        self.matches(codec).any(Entry::is_encode)
    }

    /// Required decode codecs this machine cannot handle.
    #[must_use]
    pub fn missing_required_decode(&self) -> Vec<Codec> {
        REQUIRED_DECODE
            .iter()
            .copied()
            .filter(|c| !self.can_decode(*c))
            .collect()
    }
}

/// Why probing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// `vainfo` is not installed.
    ToolMissing,
    /// `vainfo` ran but could not initialise a driver. Carries its own explanation.
    DriverFailed(String),
    /// `vainfo` ran and initialised, but reported no usable profiles at all.
    NoProfiles,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMissing => f.write_str("vainfo is not installed"),
            Self::DriverFailed(detail) => write!(f, "VA-API driver failed to load: {detail}"),
            Self::NoProfiles => f.write_str("VA-API loaded but reported no usable profiles"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Extracts the driver-failure reason from `vainfo` output, if it failed.
fn failure_reason(output: &str) -> Option<String> {
    let failed = output.contains("va_openDriver() returns -")
        || output.contains("vaInitialize failed")
        || output.contains("failed to open");
    if !failed {
        return None;
    }
    // Prefer the line that names the driver file, since a missing or mismatched
    // `*_drv_video.so` is the usual cause and the filename is the actionable part.
    let detail = output
        .lines()
        .find(|l| l.contains("Trying to open") || l.contains("vaInitialize failed"))
        .unwrap_or("no further detail reported")
        .trim();
    Some(detail.to_owned())
}

/// Parses `vainfo` output.
///
/// # Errors
/// Returns [`ProbeError::DriverFailed`] when the output shows the driver did not load,
/// or [`ProbeError::NoProfiles`] when it loaded but exposed nothing usable.
pub fn parse(output: &str) -> Result<Capabilities, ProbeError> {
    if let Some(reason) = failure_reason(output) {
        return Err(ProbeError::DriverFailed(reason));
    }

    let mut api_version = None;
    let mut driver = None;
    let mut entries = Vec::new();

    for line in output.lines() {
        let line = line.trim();

        if let Some(rest) = line.strip_prefix("vainfo: VA-API version:") {
            api_version = Some(rest.trim().to_owned());
            continue;
        }
        if let Some(rest) = line.strip_prefix("vainfo: Driver version:") {
            driver = Some(rest.trim().to_owned());
            continue;
        }

        // Profile lines look like `VAProfileH264High : VAEntrypointVLD`.
        if !line.starts_with("VAProfile") {
            continue;
        }
        if let Some((profile, entrypoint)) = line.split_once(':') {
            let entrypoint = entrypoint.trim();
            if entrypoint.starts_with("VAEntrypoint") {
                entries.push(Entry {
                    profile: profile.trim().to_owned(),
                    entrypoint: entrypoint.to_owned(),
                });
            }
        }
    }

    if entries.is_empty() {
        return Err(ProbeError::NoProfiles);
    }

    Ok(Capabilities {
        api_version,
        driver,
        entries,
    })
}

/// Runs `vainfo` against a device and parses the result.
///
/// `LIBVA_DRIVER_NAME` is not set here. The probe deliberately exercises the same
/// default driver resolution Plex will get, because a configuration that only works
/// with an explicit override is not a configuration that works.
///
/// # Errors
/// Returns [`ProbeError::ToolMissing`] if `vainfo` is absent, or a parse error.
pub fn probe(env: &impl Environment, gpu: &Gpu) -> Result<Capabilities, ProbeError> {
    if gpu.preferred_driver() == VaapiDriver::None {
        return Err(ProbeError::DriverFailed(
            "device has no VA-API driver; NVENC is not supported in this release".to_owned(),
        ));
    }
    let display = gpu.render_node.to_string_lossy().into_owned();
    let output = env
        .run("vainfo", &["--display", "drm", "--device", &display])
        .map_err(|_| ProbeError::ToolMissing)?;
    parse(&output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Fixture;
    use crate::gpu::discover;

    const WORKING: &str = include_str!("../tests/fixtures/vainfo-adl-n-ihd.txt");

    #[test]
    fn parses_a_working_intel_stack() {
        let caps = parse(WORKING).unwrap();

        assert_eq!(caps.api_version.as_deref(), Some("1.20 (libva 2.20.0)"));
        assert!(caps.driver.as_deref().unwrap().contains("iHD"));
        assert!(!caps.entries.is_empty());
    }

    #[test]
    fn identifies_the_capabilities_plex_needs() {
        let caps = parse(WORKING).unwrap();

        assert!(caps.can_decode(Codec::H264));
        assert!(caps.can_decode(Codec::Hevc));
        assert!(caps.can_decode(Codec::Hevc10));
        assert!(caps.can_encode(Codec::H264), "Plex transcodes to H.264");
        assert!(caps.missing_required_decode().is_empty());
    }

    #[test]
    fn does_not_confuse_hevc_10_bit_support_for_8_bit() {
        // A machine listing only Main10 must not be reported as decoding 8-bit HEVC.
        let caps = parse(
            "vainfo: Supported profile and entrypoints\n\
             VAProfileHEVCMain10 : VAEntrypointVLD\n",
        )
        .unwrap();

        assert!(caps.can_decode(Codec::Hevc10));
        assert!(!caps.can_decode(Codec::Hevc));
        assert_eq!(
            caps.missing_required_decode(),
            vec![Codec::H264, Codec::Hevc]
        );
    }

    #[test]
    fn treats_low_power_encode_as_encode() {
        // Modern Intel exposes H.264 encode only as EncSliceLP. Missing this would
        // report every current iGPU as encode-incapable.
        let caps = parse(
            "vainfo: Supported profile and entrypoints\n\
             VAProfileH264High : VAEntrypointEncSliceLP\n",
        )
        .unwrap();
        assert!(caps.can_encode(Codec::H264));
    }

    #[test]
    fn reports_a_driver_that_failed_to_load_with_its_reason() {
        let output = "libva info: Trying to open /usr/lib/dri/iHD_drv_video.so\n\
                      libva info: va_openDriver() returns -1\n";
        match parse(output) {
            Err(ProbeError::DriverFailed(detail)) => {
                assert!(detail.contains("iHD_drv_video.so"), "got: {detail}");
            }
            other => panic!("expected a driver failure, got {other:?}"),
        }
    }

    #[test]
    fn distinguishes_loaded_but_empty_from_failed_to_load() {
        assert_eq!(
            parse("vainfo: VA-API version: 1.20\n").unwrap_err(),
            ProbeError::NoProfiles
        );
    }

    #[test]
    fn ignores_libva_chatter_and_environment_warnings() {
        let noisy = format!("error: XDG_RUNTIME_DIR not set in the environment.\n{WORKING}");
        assert!(parse(&noisy).is_ok());
    }

    #[test]
    fn a_missing_vainfo_is_reported_as_a_missing_tool() {
        let fixture = Fixture::new().render_node("renderD128", "i915", 0x8086, 0x46d0);
        let gpu = discover(&fixture).into_iter().next().unwrap();
        assert_eq!(probe(&fixture, &gpu).unwrap_err(), ProbeError::ToolMissing);
    }

    #[test]
    fn probing_nvidia_explains_itself_rather_than_running_vainfo() {
        let fixture = Fixture::new()
            .render_node("renderD128", "nvidia", 0x10de, 0x1e84)
            .command("vainfo", WORKING);
        let gpu = discover(&fixture).into_iter().next().unwrap();

        match probe(&fixture, &gpu) {
            Err(ProbeError::DriverFailed(detail)) => assert!(detail.contains("NVENC")),
            other => panic!("expected an explanatory failure, got {other:?}"),
        }
    }

    #[test]
    fn probes_through_the_environment_when_vainfo_is_present() {
        let fixture = Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x46d0)
            .command("vainfo", WORKING);
        let gpu = discover(&fixture).into_iter().next().unwrap();

        assert!(probe(&fixture, &gpu).unwrap().can_encode(Codec::H264));
    }
}
