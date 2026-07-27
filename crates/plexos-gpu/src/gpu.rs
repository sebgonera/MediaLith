//! GPU discovery and VA-API driver selection.
//!
//! Selection is **probe-driven, not table-driven**. Mapping PCI device IDs to hardware
//! generations to VA-API drivers is how this is usually done, and it is why new
//! hardware so often falls back to software transcoding on a distribution that has not
//! been updated: the table does not know the device yet, so it guesses wrong and says
//! nothing.
//!
//! Instead: pick the driver the kernel driver implies, then *verify by probing it*
//! ([`crate::vainfo`]). An unknown device ID is not a failure — it is a device we try
//! anyway and report honestly about. The only table here is cosmetic, used to print a
//! recognisable name, and nothing depends on it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::env::Environment;

/// Where the kernel exposes DRM devices.
const DRM_CLASS: &str = "/sys/class/drm";

/// PCI vendor of a graphics device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vendor {
    /// Intel.
    Intel,
    /// AMD.
    Amd,
    /// NVIDIA.
    Nvidia,
    /// Anything else, by PCI vendor ID.
    Other(u16),
}

impl Vendor {
    /// Classifies a PCI vendor ID.
    #[must_use]
    pub const fn from_pci_id(id: u16) -> Self {
        match id {
            0x8086 => Self::Intel,
            0x1002 | 0x1022 => Self::Amd,
            0x10de => Self::Nvidia,
            other => Self::Other(other),
        }
    }
}

/// A VA-API driver implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaapiDriver {
    /// `iHD`, the Intel media driver. Skylake and newer.
    IntelMedia,
    /// `i965`, the legacy Intel driver. Pre-Skylake, outside PlexOS's target.
    IntelLegacy,
    /// `radeonsi`, via Mesa.
    RadeonSi,
    /// No VA-API driver applies; NVIDIA transcoding goes through NVENC instead.
    None,
}

impl VaapiDriver {
    /// The value to set as `LIBVA_DRIVER_NAME`, if any.
    #[must_use]
    pub const fn libva_name(self) -> Option<&'static str> {
        match self {
            Self::IntelMedia => Some("iHD"),
            Self::IntelLegacy => Some("i965"),
            Self::RadeonSi => Some("radeonsi"),
            Self::None => None,
        }
    }
}

/// A discovered graphics device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    /// DRM node name, e.g. `renderD128`.
    pub node: String,
    /// Path Plex and FFmpeg open for hardware acceleration.
    pub render_node: PathBuf,
    /// PCI vendor.
    pub vendor: Vendor,
    /// PCI device ID.
    pub device_id: u16,
    /// Kernel driver bound to the device, e.g. `i915`, `xe`, `amdgpu`.
    pub kernel_driver: String,
    /// Recognisable hardware name, when the device is one we happen to know.
    ///
    /// Cosmetic only. Nothing depends on this being populated.
    pub model: Option<String>,
}

impl Gpu {
    /// The VA-API driver to try for this device.
    ///
    /// A starting point to be verified by probing, not a conclusion. `xe` only ever
    /// binds to hardware new enough for the Intel media driver; `i915` spans both, and
    /// the modern driver is the right first guess for anything PlexOS targets.
    #[must_use]
    pub fn preferred_driver(&self) -> VaapiDriver {
        match self.vendor {
            Vendor::Intel => VaapiDriver::IntelMedia,
            Vendor::Amd => VaapiDriver::RadeonSi,
            Vendor::Nvidia | Vendor::Other(_) => VaapiDriver::None,
        }
    }

    /// Whether this device is one PlexOS supports for hardware transcoding.
    #[must_use]
    pub fn is_supported_target(&self) -> bool {
        matches!(self.vendor, Vendor::Intel | Vendor::Amd)
    }
}

/// Cosmetic device names. Absence from this list means nothing, and neither does an
/// entry being wrong — nothing depends on it.
///
/// Provenance is recorded per entry, because these are exactly the sort of constant
/// that gets copied from a forum post and never checked. `confirmed` means a capture
/// from that machine is in the repository; `documented` means it came from a vendor
/// list and nobody has run PlexOS on one.
const KNOWN_MODELS: &[(u16, u16, &str)] = &[
    // confirmed — tools/captures/huawei-wrt-wx9.txt
    (0x8086, 0x3ea0, "Intel UHD Graphics 620 (Whiskey Lake-U)"),
    // documented, unverified
    (0x8086, 0x5917, "Intel UHD Graphics 620 (Kaby Lake-R)"),
    (0x8086, 0x3e9b, "Intel UHD Graphics 630 (Coffee Lake-H)"),
    (0x8086, 0x46d0, "Intel Alder Lake-N (N100 class)"),
    (0x8086, 0x46d1, "Intel Alder Lake-N (N200 class)"),
];

fn model_name(vendor: u16, device: u16) -> Option<String> {
    KNOWN_MODELS
        .iter()
        .find(|(v, d, _)| *v == vendor && *d == device)
        .map(|(_, _, name)| (*name).to_owned())
}

/// Parses a sysfs hex attribute such as `0x8086\n`.
fn parse_hex_id(raw: &str) -> Option<u16> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u16::from_str_radix(digits, 16).ok()
}

/// Discovers every render-capable GPU on the system.
///
/// Render nodes rather than card nodes: `renderD*` is what an unprivileged process
/// opens for offscreen work, and Plex runs unprivileged. A card node without a
/// matching render node cannot be used for transcoding no matter what it supports.
pub fn discover(env: &impl Environment) -> Vec<Gpu> {
    let Ok(entries) = env.list_dir(Path::new(DRM_CLASS)) else {
        return Vec::new();
    };

    let mut gpus: Vec<Gpu> = entries
        .iter()
        .filter_map(|entry| {
            let node = entry.file_name()?.to_str()?;
            if !node.starts_with("renderD") {
                return None;
            }
            let device = entry.join("device");
            let vendor_id = parse_hex_id(&env.read(&device.join("vendor")).ok()?)?;
            let device_id = parse_hex_id(&env.read(&device.join("device")).ok()?)?;
            let kernel_driver = env
                .read_link(&device.join("driver"))
                .ok()
                .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "unknown".to_owned());

            Some(Gpu {
                node: node.to_owned(),
                render_node: PathBuf::from(format!("/dev/dri/{node}")),
                vendor: Vendor::from_pci_id(vendor_id),
                device_id,
                kernel_driver,
                model: model_name(vendor_id, device_id),
            })
        })
        .collect();

    // Deterministic order so a report from the same machine is byte-identical between
    // runs; a diagnostic that reorders itself is a diagnostic nobody can diff.
    gpus.sort_by(|a, b| a.node.cmp(&b.node));
    gpus
}

/// Picks the device to transcode on.
///
/// Intel first, then AMD. Not a claim that Intel is faster in general — it is that
/// QuickSync on an Intel iGPU is the configuration PlexOS targets and tests, and on a
/// machine with both, that is the one more likely to work unattended.
#[must_use]
pub fn select_primary(gpus: &[Gpu]) -> Option<&Gpu> {
    gpus.iter()
        .find(|g| g.vendor == Vendor::Intel)
        .or_else(|| gpus.iter().find(|g| g.vendor == Vendor::Amd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Fixture;

    #[test]
    fn finds_an_intel_igpu_on_an_n100_class_machine() {
        let fixture = Fixture::new().render_node("renderD128", "i915", 0x8086, 0x46d0);
        let gpus = discover(&fixture);

        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.vendor, Vendor::Intel);
        assert_eq!(gpu.device_id, 0x46d0);
        assert_eq!(gpu.kernel_driver, "i915");
        assert_eq!(gpu.render_node, PathBuf::from("/dev/dri/renderD128"));
        assert_eq!(gpu.preferred_driver(), VaapiDriver::IntelMedia);
        assert_eq!(gpu.preferred_driver().libva_name(), Some("iHD"));
        assert!(gpu.is_supported_target());
    }

    #[test]
    fn an_unknown_intel_device_id_is_still_a_usable_target() {
        // The point of probe-driven selection: hardware released after this build must
        // not be reported as unsupported merely because no table lists it.
        let fixture = Fixture::new().render_node("renderD128", "xe", 0x8086, 0xfffe);
        let gpu = &discover(&fixture)[0];

        assert_eq!(gpu.model, None, "unknown device, so no cosmetic name");
        assert!(gpu.is_supported_target());
        assert_eq!(gpu.preferred_driver(), VaapiDriver::IntelMedia);
    }

    #[test]
    fn ignores_card_nodes_and_keeps_only_render_nodes() {
        let fixture = Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x46d0)
            .file("/sys/class/drm/card0/device/vendor", "0x8086\n")
            .file("/sys/class/drm/card0/device/device", "0x46d0\n");

        let gpus = discover(&fixture);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].node, "renderD128");
    }

    #[test]
    fn prefers_intel_when_a_discrete_card_is_also_present() {
        let fixture = Fixture::new()
            .render_node("renderD128", "i915", 0x8086, 0x46d0)
            .render_node("renderD129", "amdgpu", 0x1002, 0x1636);

        let gpus = discover(&fixture);
        assert_eq!(gpus.len(), 2);
        assert_eq!(select_primary(&gpus).unwrap().vendor, Vendor::Intel);
    }

    #[test]
    fn nvidia_is_discovered_but_not_a_vaapi_target() {
        let fixture = Fixture::new().render_node("renderD128", "nvidia", 0x10de, 0x1e84);
        let gpu = &discover(&fixture)[0];

        assert_eq!(gpu.vendor, Vendor::Nvidia);
        assert!(!gpu.is_supported_target(), "NVENC is out of scope for v1");
        assert_eq!(gpu.preferred_driver().libva_name(), None);
        assert!(select_primary(&discover(&fixture)).is_none());
    }

    #[test]
    fn a_headless_machine_yields_no_gpus_rather_than_an_error() {
        assert!(discover(&Fixture::new()).is_empty());
    }

    #[test]
    fn a_device_missing_its_driver_link_is_still_reported() {
        // A device with no bound driver is exactly the case worth reporting: the
        // hardware is present and unusable, which is invisible if discovery skips it.
        let fixture = Fixture::new()
            .file("/sys/class/drm/renderD128/device/vendor", "0x8086\n")
            .file("/sys/class/drm/renderD128/device/device", "0x46d0\n");

        let gpus = discover(&fixture);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].kernel_driver, "unknown");
    }

    #[test]
    fn parses_sysfs_hex_attributes() {
        assert_eq!(parse_hex_id("0x8086\n"), Some(0x8086));
        assert_eq!(parse_hex_id("8086"), Some(0x8086));
        assert_eq!(parse_hex_id("  0x46d0  "), Some(0x46d0));
        assert_eq!(parse_hex_id("nonsense"), None);
        assert_eq!(parse_hex_id(""), None);
    }

    #[test]
    fn discovery_order_is_stable() {
        let fixture = Fixture::new()
            .render_node("renderD129", "amdgpu", 0x1002, 0x1636)
            .render_node("renderD128", "i915", 0x8086, 0x46d0);
        let gpus = discover(&fixture);
        let nodes: Vec<&str> = gpus.iter().map(|g| g.node.as_str()).collect();
        assert_eq!(nodes, ["renderD128", "renderD129"]);
    }
}
