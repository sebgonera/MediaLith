//! The boundary between inspection logic and the machine being inspected.
//!
//! Everything this crate concludes is derived from four operations: listing a
//! directory, reading a file, resolving a symlink, and running a command. Putting them
//! behind [`Environment`] means the entire decision path — which driver to use, whether
//! firmware loaded, whether the capabilities are sufficient — is testable against
//! recorded fixtures from real machines, with no GPU present.
//!
//! That matters more here than it would elsewhere. This crate exists to diagnose
//! hardware we do not have in front of us, on machines we cannot log into, and the
//! logic has to be trustworthy on hardware nobody has tested it against.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Read-only access to the machine under inspection.
pub trait Environment {
    /// Lists the entries of a directory. Order is not guaranteed.
    ///
    /// # Errors
    /// Fails if the directory does not exist or cannot be read.
    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;

    /// Reads a file to a string.
    ///
    /// # Errors
    /// Fails if the file does not exist or cannot be read.
    fn read(&self, path: &Path) -> io::Result<String>;

    /// Resolves a symlink to its target, which may be relative.
    ///
    /// # Errors
    /// Fails if the path is not a symlink or cannot be read.
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;

    /// The permission bits of a path, if it exists.
    ///
    /// Exists for one question: whether the account Plex runs as can open the render
    /// node. Everything else this crate asks is answered by a root process about the
    /// hardware, and a report that says "ready" while the device is root-only is
    /// answering about the wrong process.
    ///
    /// Returns `None` rather than an error for a path that is not there, because a
    /// machine with no render node is a case the report already covers by name.
    ///
    /// Defaulted to `None` so that a test double describing a network interface does not
    /// have to invent an answer about file permissions. `None` means "no opinion", and
    /// the one caller treats that as nothing to report rather than as a problem.
    fn mode(&self, path: &Path) -> Option<u32> {
        let _ = path;
        None
    }

    /// Runs a command and returns its combined stdout and stderr.
    ///
    /// `vainfo` writes some of its most useful output to stderr, so the two are merged
    /// rather than kept apart.
    ///
    /// # Errors
    /// Fails if the program is not installed or could not be executed. A non-zero exit
    /// status is *not* an error: a failing `vainfo` still prints the reason, and that
    /// reason is exactly what this crate is trying to report.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<String>;
}

/// Inspects the running system.
#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Environment for System {
    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|e| e.path())
            .collect();
        entries.sort();
        Ok(entries)
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn mode(&self, path: &Path) -> Option<u32> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).ok().map(|m| m.permissions().mode())
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(path)
    }

    fn run(&self, program: &str, args: &[&str]) -> io::Result<String> {
        let output = std::process::Command::new(program).args(args).output()?;
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(combined)
    }
}

/// A recorded machine, for tests.
///
/// Build one from a capture of a real system's sysfs and command output, and the whole
/// diagnostic path runs against it deterministically.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    files: BTreeMap<PathBuf, String>,
    links: BTreeMap<PathBuf, PathBuf>,
    commands: BTreeMap<String, String>,
    modes: BTreeMap<PathBuf, u32>,
}

impl Fixture {
    /// An empty machine: no DRM devices, no tools installed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a readable file.
    #[must_use]
    pub fn file(mut self, path: impl Into<PathBuf>, contents: impl Into<String>) -> Self {
        self.files.insert(path.into(), contents.into());
        self
    }

    /// Adds a symlink.
    #[must_use]
    pub fn link(mut self, path: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        self.links.insert(path.into(), target.into());
        self
    }

    /// Records the permission bits of a path.
    #[must_use]
    pub fn mode(mut self, path: impl Into<PathBuf>, mode: u32) -> Self {
        self.modes.insert(path.into(), mode);
        self
    }

    /// Records the output of a command, keyed by the program name.
    #[must_use]
    pub fn command(mut self, program: impl Into<String>, output: impl Into<String>) -> Self {
        self.commands.insert(program.into(), output.into());
        self
    }

    /// Adds a DRM render node with its backing PCI attributes.
    ///
    /// A convenience for the shape every Intel and AMD system has, so tests read as
    /// descriptions of hardware rather than as sysfs trivia.
    #[must_use]
    pub fn render_node(self, node: &str, driver: &str, vendor: u16, device: u16) -> Self {
        let base = format!("/sys/class/drm/{node}/device");
        self.file(format!("{base}/vendor"), format!("0x{vendor:04x}\n"))
            .file(format!("{base}/device"), format!("0x{device:04x}\n"))
            .link(
                format!("{base}/driver"),
                format!("../../../bus/pci/drivers/{driver}"),
            )
            .file(format!("/dev/dri/{node}"), String::new())
    }
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: not found", path.display()),
    )
}

impl Environment for Fixture {
    fn mode(&self, path: &Path) -> Option<u32> {
        // A recorded file with no recorded mode is 0666, which is what a render node
        // looks like on any machine with a udev rule -- so a fixture that does not care
        // about permissions does not accidentally assert the broken case.
        self.modes
            .get(path)
            .copied()
            .or_else(|| self.files.contains_key(path).then_some(0o666))
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let prefix = path.to_string_lossy();
        let prefix = format!("{}/", prefix.trim_end_matches('/'));
        let mut entries: Vec<PathBuf> = self
            .files
            .keys()
            .chain(self.links.keys())
            .filter_map(|p| {
                let s = p.to_string_lossy();
                let rest = s.strip_prefix(&prefix)?;
                let head = rest.split('/').next()?;
                Some(PathBuf::from(format!("{prefix}{head}")))
            })
            .collect();
        entries.sort();
        entries.dedup();
        if entries.is_empty() {
            return Err(not_found(path));
        }
        Ok(entries)
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        self.files.get(path).cloned().ok_or_else(|| not_found(path))
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.links.get(path).cloned().ok_or_else(|| not_found(path))
    }

    fn run(&self, program: &str, args: &[&str]) -> io::Result<String> {
        let _ = args;
        self.commands.get(program).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{program}: command not found"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_lists_only_immediate_children() {
        let fixture = Fixture::new()
            .file("/sys/class/drm/card0/device/vendor", "0x8086")
            .file("/sys/class/drm/renderD128/device/vendor", "0x8086");
        let entries = fixture.list_dir(Path::new("/sys/class/drm")).unwrap();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/sys/class/drm/card0"),
                PathBuf::from("/sys/class/drm/renderD128"),
            ]
        );
    }

    #[test]
    fn fixture_reports_missing_paths_rather_than_empty_results() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture
                .list_dir(Path::new("/sys/class/drm"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(fixture.read(Path::new("/nope")).is_err());
        assert!(fixture.run("vainfo", &[]).is_err());
    }

    #[test]
    fn render_node_helper_builds_the_expected_shape() {
        let fixture = Fixture::new().render_node("renderD128", "i915", 0x8086, 0x46d1);
        assert_eq!(
            fixture
                .read(Path::new("/sys/class/drm/renderD128/device/vendor"))
                .unwrap()
                .trim(),
            "0x8086"
        );
        assert!(
            fixture
                .read_link(Path::new("/sys/class/drm/renderD128/device/driver"))
                .unwrap()
                .ends_with("i915")
        );
    }
}
