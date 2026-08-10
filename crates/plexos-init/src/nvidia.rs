//! Bringing an NVIDIA card up, on a system with no `udev` and no `modprobe`.
//!
//! ADR-0015 step 2 said something in PlexOS would have to create `/dev/nvidia*` if the
//! open modules did not register through the device model. They do not, and this is that
//! something.
//!
//! # What the machine actually said
//!
//! Loaded by hand on an RTX 5060, `/proc/devices` gave:
//!
//! ```text
//! 195 nvidia
//! 195 nvidiactl
//! 242 nvidia-nvswitch
//! 243 nvidia-nvlink
//! 244 nvidia-caps
//! ```
//!
//! and `/dev` had nothing in it. Both halves confirmed: the major matches the constant in
//! `nv-chardev-numbers.h`, and `devtmpfs` created no node for any of it.
//!
//! # Why the majors are read rather than assumed
//!
//! `nvidia` and `nvidiactl` are 195 by compile-time constant. `nvidia-uvm` is not:
//! `uvm.c` takes its major with `alloc_chrdev_region`, so the kernel assigns it at load
//! time and it can differ between boots. Hard-coding what one machine happened to give
//! would be the "tests that only compare a thing to itself" trap in a new place -- right
//! until the day something else took the number first.
//!
//! # Only when there is a card
//!
//! Two of the three machines this runs on are Intel laptops. Loading a 27 MB module on
//! them to bind nothing would cost memory, taint the kernel, and put a line in the log
//! that reads like a fault. The PCI bus is asked first.
//!
//! # What has run
//!
//! **This module has not run on a machine.** Every step it performs has been done by hand
//! on the RTX 5060 -- the modules load, the kernel accepts their signature, `/proc/devices`
//! reports the majors -- but this code has not done it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// NVIDIA's PCI vendor ID.
pub const VENDOR: &str = "0x10de";

/// Where the PCI bus is enumerated.
const PCI_DEVICES: &str = "/sys/bus/pci/devices";

/// The compile-time major, from `nv-chardev-numbers.h`.
pub const NV_MAJOR: u32 = 195;

/// `NV_MINOR_DEVICE_NUMBER_CONTROL_DEVICE`.
pub const CONTROL_MINOR: u32 = 255;

/// Modules, in the order they must be loaded: everything depends on `nvidia`.
///
/// `nvidia-drm` is included because Plex's use of NVDEC goes through it on modern
/// releases; `nvidia-peermem` is not built at all.
pub const MODULES: [&str; 4] = ["nvidia", "nvidia-uvm", "nvidia-modeset", "nvidia-drm"];

/// A device node that has to be created because nothing else will.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Where it goes.
    pub path: PathBuf,
    /// Character device major.
    pub major: u32,
    /// Character device minor.
    pub minor: u32,
    /// Permission bits.
    ///
    /// `0666`, and not as a shrug. Without udev a node is exactly what creates it, and
    /// Plex does not run as root -- the render node had to be relaxed for the same reason
    /// and the failure was invisible, because every probe above it ran as root and
    /// reported success while Plex quietly used the CPU.
    pub mode: u32,
}

/// Whether this machine has an NVIDIA card on the PCI bus.
///
/// Reads the vendor of every device rather than looking for a driver: the whole point is
/// to decide *before* a driver exists.
pub fn present(env: &impl plexos_gpu::env::Environment) -> bool {
    let Ok(devices) = env.list_dir(Path::new(PCI_DEVICES)) else {
        return false;
    };
    devices.iter().any(|device| {
        env.read(&device.join("vendor"))
            .is_ok_and(|vendor| vendor.trim().eq_ignore_ascii_case(VENDOR))
    })
}

/// Character-device majors, from the contents of `/proc/devices`.
///
/// The file has a block section after the character one and both are `major name` pairs,
/// so parsing stops at the blank line that separates them. Reading past it would report a
/// block major as a character one, and a node made with the wrong one opens something
/// else entirely.
#[must_use]
pub fn majors(proc_devices: &str) -> BTreeMap<String, u32> {
    let mut found = BTreeMap::new();
    for line in proc_devices.lines() {
        let line = line.trim();
        if line.eq_ignore_ascii_case("Block devices:") {
            break;
        }
        let Some((number, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let (Ok(major), name) = (number.trim().parse::<u32>(), name.trim()) else {
            continue;
        };
        if !name.is_empty() {
            found.insert(name.to_owned(), major);
        }
    }
    found
}

/// The nodes to create, given what `/proc/devices` reported.
///
/// `nvidia0` is minor 0 because this is one card. A second card would be minor 1, and
/// this deliberately does not guess at that: the appliance has one GPU doing the
/// transcoding, and inventing nodes for hardware nobody has is how a list stops matching
/// a machine.
#[must_use]
pub fn nodes(majors: &BTreeMap<String, u32>) -> Vec<Node> {
    let mut nodes = Vec::new();

    // 195 by constant, and confirmed against /proc/devices when it is there. If the two
    // ever disagree, the kernel is right and the constant is what changed upstream.
    let control_major = majors.get("nvidiactl").copied().unwrap_or(NV_MAJOR);
    let card_major = majors.get("nvidia").copied().unwrap_or(NV_MAJOR);

    nodes.push(Node {
        path: PathBuf::from("/dev/nvidiactl"),
        major: control_major,
        minor: CONTROL_MINOR,
        mode: 0o666,
    });
    nodes.push(Node {
        path: PathBuf::from("/dev/nvidia0"),
        major: card_major,
        minor: 0,
        mode: 0o666,
    });

    // The one that cannot be assumed. Absent if nvidia-uvm did not load, and then no node
    // is made rather than one pointing at whatever holds that number instead.
    if let Some(&major) = majors.get("nvidia-uvm") {
        nodes.push(Node {
            path: PathBuf::from("/dev/nvidia-uvm"),
            major,
            minor: 0,
            mode: 0o666,
        });
        nodes.push(Node {
            path: PathBuf::from("/dev/nvidia-uvm-tools"),
            major,
            minor: 1,
            mode: 0o666,
        });
    }

    nodes
}

/// Where a module lives in the running system.
#[must_use]
pub fn module_path(release: &str, name: &str) -> PathBuf {
    PathBuf::from(format!("/usr/lib/modules/{release}/extra/{name}.ko"))
}

/// Loads the modules and makes the nodes, if this machine has a card.
///
/// Never fatal, and that is deliberate: an appliance whose GPU did not come up must
/// still boot, serve its console and say what happened. A machine that refused to start
/// over a graphics card would be unable to tell anybody why.
///
/// Every outcome is logged including the dull one. "No NVIDIA card, nothing to do" is
/// worth a line, because otherwise a machine with a card that failed to enumerate and a
/// machine with no card at all look identical from the console — which is the shape of
/// the render-node defect, where success and failure printed the same nothing.
pub fn bring_up(env: &impl plexos_gpu::env::Environment, log: &mut dyn FnMut(&str)) {
    if !present(env) {
        log("nvidia: no card on the PCI bus, nothing to load");
        return;
    }

    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|r| r.trim().to_owned())
        .unwrap_or_default();
    if release.is_empty() {
        log("nvidia: could not read the kernel release, so the module path is unknown");
        return;
    }

    for name in MODULES {
        let path = module_path(&release, name);
        match plexos_sys::module::load(&path, "") {
            Ok(()) => log(&format!("nvidia: loaded {name}")),
            // Already there. A second boot of the same supervisor, or somebody ahead of
            // us; either way the node work below is still worth doing.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                log(&format!("nvidia: {name} was already loaded"));
            }
            Err(error) => {
                log(&format!(
                    "nvidia: could not load {name} from {}: {error}. {}",
                    path.display(),
                    remedy_for(&error)
                ));
                // No point continuing: everything after nvidia depends on it.
                if name == "nvidia" {
                    return;
                }
            }
        }
    }

    // Read *after* loading. The uvm major does not exist until its module does.
    let devices = std::fs::read_to_string("/proc/devices").unwrap_or_default();
    let found = majors(&devices);
    if !found.contains_key("nvidia") {
        log(
            "nvidia: the modules loaded but /proc/devices lists no nvidia major, so no \
             nodes were made. Nothing else here can explain that; read dmesg.",
        );
        return;
    }

    for node in nodes(&found) {
        match plexos_sys::module::make_char_node(&node.path, node.major, node.minor, node.mode) {
            Ok(()) => log(&format!(
                "nvidia: {} is {}:{} mode {:o}",
                node.path.display(),
                node.major,
                node.minor,
                node.mode
            )),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => log(&format!(
                "nvidia: could not create {}: {error}. Plex will not find the GPU and \
                 will transcode on the CPU without saying so.",
                node.path.display()
            )),
        }
    }
}

/// `EKEYREJECTED`, spelled out because this crate does not link libc — that boundary is
/// what `plexos-sys` exists for, and one integer is not a reason to cross it.
const EKEYREJECTED: i32 = 129;

/// `ENOEXEC`.
const ENOEXEC: i32 = 8;

/// `ENOENT`.
const ENOENT: i32 = 2;

/// The remedy that matches the errno, rather than the operation.
///
/// The trap list has this as its own entry: `could not bind :80` first suggested "pass a
/// higher port", which is right for `EACCES` and actively misleading for `EADDRINUSE`.
/// The same applies here, and more sharply — a refused signature and a missing file both
/// produce "the card does nothing", and they are fixed in completely different places.
fn remedy_for(error: &std::io::Error) -> &'static str {
    match error.raw_os_error() {
        // MODULE_SIG_FORCE is on, so this is a module this kernel did not sign -- an
        // entirely different problem from a missing or broken file, and the one somebody
        // would otherwise go looking at the hardware for.
        Some(EKEYREJECTED) => {
            "The kernel refused its signature. CONFIG_MODULE_SIG_FORCE is set, so a \
             module has to be signed with this kernel's own key; one built elsewhere \
             will always be refused. Rebuild plexos-nvidia against this image."
        }
        Some(ENOEXEC) => {
            "The file is not a module this kernel can load, which usually means it was \
             built against a different kernel version."
        }
        Some(ENOENT) => {
            "The module is not in the image. Check BR2_PACKAGE_PLEXOS_NVIDIA survived \
             kconfig."
        }
        _ => "Read dmesg; the kernel logs a reason this call does not carry.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_gpu::env::Fixture;

    /// Captured from the RTX 5060 after loading nvidia.ko by hand, rather than imagined.
    /// A fixture somebody invented is a test that agrees with the code and not with the
    /// machine -- which this project has already paid for once, over resolv.conf.
    const REAL_PROC_DEVICES: &str = "\
Character devices:
  1 mem
  4 tty
  5 /dev/tty
195 nvidia
195 nvidiactl
242 nvidia-nvswitch
243 nvidia-nvlink
244 nvidia-caps
245 nvidia-caps-imex-channels

Block devices:
259 blkext
  8 sd
";

    #[test]
    fn the_majors_come_out_of_what_the_machine_printed() {
        let found = majors(REAL_PROC_DEVICES);
        assert_eq!(found.get("nvidia"), Some(&195));
        assert_eq!(found.get("nvidiactl"), Some(&195));
        assert_eq!(found.get("nvidia-nvlink"), Some(&243));
    }

    #[test]
    fn the_block_section_is_not_read_as_character_devices() {
        // `8 sd` is a block major. Treated as a character one it would produce a node
        // that opens something else entirely, and nothing about the node would look wrong.
        let found = majors(REAL_PROC_DEVICES);
        assert_eq!(
            found.get("sd"),
            None,
            "a block major leaked into the character map"
        );
        assert_eq!(found.get("blkext"), None);
    }

    #[test]
    fn the_control_and_card_nodes_use_the_constant_the_header_defines() {
        let made = nodes(&majors(REAL_PROC_DEVICES));
        let control = made
            .iter()
            .find(|n| n.path.ends_with("nvidiactl"))
            .expect("a control node");
        assert_eq!((control.major, control.minor), (195, 255));
        let card = made
            .iter()
            .find(|n| n.path.ends_with("nvidia0"))
            .expect("a card node");
        assert_eq!((card.major, card.minor), (195, 0));
    }

    #[test]
    fn no_uvm_node_is_invented_when_the_module_did_not_load() {
        // uvm's major is allocated at load time, so with the module absent there is no
        // number to use. Guessing one produces a node pointing at whatever else holds it.
        let made = nodes(&majors(REAL_PROC_DEVICES));
        assert!(
            !made
                .iter()
                .any(|n| n.path.to_string_lossy().contains("uvm")),
            "invented a uvm node from a /proc/devices that has no uvm in it"
        );
    }

    #[test]
    fn the_uvm_node_uses_whatever_the_kernel_allocated() {
        let mut with_uvm = majors(REAL_PROC_DEVICES);
        with_uvm.insert("nvidia-uvm".to_owned(), 511);
        let made = nodes(&with_uvm);
        let uvm = made
            .iter()
            .find(|n| n.path.ends_with("nvidia-uvm"))
            .expect("a uvm node once the module is loaded");
        assert_eq!(uvm.major, 511, "the dynamic major must not be hard-coded");
    }

    #[test]
    fn every_node_is_reachable_by_the_account_plex_runs_as() {
        // The render-node defect in one line. Root-only nodes are the state in which
        // every probe succeeds and only Plex fails, quietly, on the CPU.
        let mut all = majors(REAL_PROC_DEVICES);
        all.insert("nvidia-uvm".to_owned(), 511);
        for node in nodes(&all) {
            assert_eq!(
                node.mode,
                0o666,
                "{} is not reachable by uid 900",
                node.path.display()
            );
        }
    }

    #[test]
    fn a_machine_with_no_nvidia_card_is_left_alone() {
        let env = Fixture::new()
            .file("/sys/bus/pci/devices/0000:00:02.0/vendor", "0x8086\n")
            .file("/sys/bus/pci/devices/0000:03:00.0/vendor", "0x10ec\n");
        assert!(!present(&env), "Intel and Realtek were mistaken for NVIDIA");
    }

    #[test]
    fn a_machine_with_one_is_found() {
        let env = Fixture::new()
            .file("/sys/bus/pci/devices/0000:00:02.0/vendor", "0x8086\n")
            .file("/sys/bus/pci/devices/0000:01:00.0/vendor", "0x10de\n");
        assert!(present(&env));
    }

    #[test]
    fn the_module_path_is_where_the_package_installs_them() {
        assert_eq!(
            module_path("6.19.14", "nvidia"),
            PathBuf::from("/usr/lib/modules/6.19.14/extra/nvidia.ko")
        );
    }
}
