//! Starting the Plex that was provisioned.
//!
//! `plexos_plex::run::confine_and_exec` has held the whole confinement sequence — join
//! the cgroup, apply Landlock, drop to uid 900, `execve` — since it was written, and
//! nothing called it. This is the caller: it creates the directories Plex owns, sets the
//! cgroup up, and starts a child that confines itself.
//!
//! # Two processes, and why
//!
//! The confinement has to apply to Plex and to nothing else, and the obvious tool —
//! `CommandExt::pre_exec` — is `unsafe`, which this crate forbids. So the child is a
//! fresh `plexosd` invoked with [`CHILD_FLAG`], and it confines *itself* before `exec`ing
//! Plex. `plexos_plex::run`'s module documentation gives the full argument. The practical
//! consequence is worth knowing when this misbehaves: every step the child takes is an
//! ordinary call in an ordinary process, so the whole sequence can be run by hand from a
//! shell.
//!
//! # What this is not
//!
//! It is not a supervisor. It starts Plex once and notices whether the child is still
//! alive; nothing restarts one that dies, and replacing a running Plex with a
//! newly-provisioned one needs a reboot. That is item 8 on the list in CLAUDE.md, and
//! pretending otherwise here would produce a console that reports a version it is not
//! running.
//!
//! # What has run
//!
//! **Nothing here has started Plex on the appliance.** The pieces below are ordinary
//! filesystem and process calls, and the confinement they lead to has never executed
//! outside a test. Delete this notice when it has.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use plexos_types::paths;

/// The argument that makes a `plexosd` invocation the confined child.
pub const CHILD_FLAG: &str = "--plex-child";

/// Mode for the directories Plex owns.
///
/// `0o700`: the media database and the transcode scratch are Plex's alone. Nothing else
/// on the appliance reads them, and the appliance has no other users to read them.
const OWNED_MODE: u32 = 0o700;

/// Where the machine reports its memory, for sizing the cgroup's bounds.
const MEMINFO: &str = "/proc/meminfo";

/// Creates what Plex needs before it can be started, and returns its cgroup.
///
/// The directories are created *and chowned*: `confine_and_exec` drops to uid 900, and a
/// data directory owned by root is a Plex that starts and then fails to write its
/// database — with a permissions error naming a file rather than this omission.
///
/// # Errors
/// If a directory cannot be created or given to Plex, or if the cgroup cannot be made at
/// all. A cgroup whose individual limits fail to apply is reported and not fatal:
/// `cgroup::apply` says which bound is missing, and running Plex unbounded beats not
/// running it.
pub fn prepare(log: &mut dyn FnMut(&str)) -> io::Result<PathBuf> {
    for directory in [paths::PLEX_DATA, paths::PLEX_TRANSCODE_DIR] {
        let path = Path::new(directory);
        std::fs::create_dir_all(path)?;
        give_to_plex(path)?;
        log(&format!(
            "{} is Plex's, {}:{}",
            path.display(),
            paths::PLEX_UID,
            paths::PLEX_GID
        ));
    }

    let meminfo = std::fs::read_to_string(MEMINFO).unwrap_or_default();
    let total = plexos_plex::cgroup::total_memory(&meminfo).unwrap_or_default();
    if total == 0 {
        // Not fatal, and not silent. The limits are fractions of what the machine has,
        // so a zero here would set bounds of zero and Plex would be killed the moment it
        // allocated anything.
        log(&format!(
            "could not read a memory total from {MEMINFO}, so no memory bound is set. \
             Plex runs unbounded, which is worth fixing but will not stop it working."
        ));
        return plexos_plex::cgroup::apply(Path::new(plexos_plex::cgroup::CGROUP_ROOT), 0, log);
    }

    plexos_plex::cgroup::apply(Path::new(plexos_plex::cgroup::CGROUP_ROOT), total, log)
}

/// Gives a directory to the Plex account, with a mode only it can use.
fn give_to_plex(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    // std::os::unix::fs::chown, not a syscall of our own: this is one of the few places
    // the standard library already provides what plexos-sys would otherwise have to.
    std::os::unix::fs::chown(path, Some(paths::PLEX_UID), Some(paths::PLEX_GID))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNED_MODE))
}

/// Whether the app image is mounted, which is what makes Plex startable at all.
///
/// Checks for the binary rather than for the mount point: an empty directory at
/// [`paths::PLEX_MOUNT`] exists whether or not anything is mounted on it, and starting a
/// child that fails to `exec` produces a worse diagnostic than declining to start.
#[must_use]
pub fn is_provisioned(mount: &Path) -> bool {
    mount
        .join(plexos_plex::run::HOME_WITHIN_IMAGE)
        .join(plexos_plex::run::BINARY)
        .exists()
}

/// A running Plex, or the absence of one.
///
/// Behind a mutex because provisioning finishes on its own thread and the console starts
/// Plex on another, and both ask the same question: is one already running.
#[derive(Debug, Default)]
pub struct Handle {
    child: Mutex<Option<std::process::Child>>,
}

impl Handle {
    /// A handle that has started nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts Plex if the app image is there and nothing is running already.
    ///
    /// Reports what it decided in every case, including the cases where it does nothing:
    /// "Plex is not running" and "Plex was never installed" are different problems with
    /// different remedies, and a silent no-op makes them look the same.
    pub fn ensure_started(&self, mount: &Path, log: &mut dyn FnMut(&str)) {
        let mut held = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(running) = held.as_mut() {
            match running.try_wait() {
                Ok(None) => {
                    log("Plex is already running");
                    return;
                }
                Ok(Some(status)) => log(&format!(
                    "Plex exited with {status}. Nothing restarts it yet, so this is a \
                     start rather than a restart -- see the supervisor item in \
                     CLAUDE.md."
                )),
                Err(error) => log(&format!("could not tell whether Plex is running: {error}")),
            }
        }

        if !is_provisioned(mount) {
            log(&format!(
                "Plex is not installed, so there is nothing to start. Install it from \
                 the console page; the app image would be mounted at {}.",
                mount.display()
            ));
            return;
        }

        match start(log) {
            Ok(child) => {
                log(&format!("Plex started as pid {}", child.id()));
                *held = Some(child);
            }
            Err(error) => log(&format!("could not start Plex: {error}")),
        }
    }

    /// Asks Plex to exit, and waits for it.
    ///
    /// `SIGTERM` and then patience, rather than `SIGKILL`: Plex keeps its library in
    /// `SQLite`, and killing it mid-write is the specific damage the shutdown sequence
    /// exists to avoid. Returns once it has gone or the grace period has run out, and
    /// says which — an appliance that reports a clean stop it did not achieve is worse
    /// than one that admits Plex ignored it.
    pub fn stop(&self, grace: std::time::Duration, log: &mut dyn FnMut(&str)) {
        let mut held = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let Some(child) = held.as_mut() else {
            log("no Plex was started from here, so there is nothing to stop");
            return;
        };

        match plexos_sys::power::terminate(child.id()) {
            Ok(()) => log(&format!("asked Plex (pid {}) to exit", child.id())),
            Err(error) => {
                log(&format!(
                    "could not signal Plex (pid {}): {error}. It has probably exited \
                     already; continuing.",
                    child.id()
                ));
                return;
            }
        }

        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => {
                    log(&format!("Plex exited with {status}"));
                    *held = None;
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
                Err(error) => {
                    log(&format!("could not wait for Plex: {error}"));
                    return;
                }
            }
        }

        log(&format!(
            "Plex has not exited after {}s. Going ahead: its data is on a journalling \
             filesystem and the alternative is refusing to turn the machine off.",
            grace.as_secs()
        ));
    }

    /// Whether a child is alive right now.
    #[must_use]
    pub fn is_running(&self) -> bool {
        let mut held = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }
}

/// Mounts a freshly-provisioned app image and starts Plex from it.
///
/// The path a first installation takes: at boot there was nothing to mount, so
/// `--mount-plex` did nothing, and without this the administrator would have to reboot
/// the appliance to use the Plex they just installed.
///
/// An *already mounted* image is left alone and said so. Replacing one under a running
/// Plex needs the running Plex stopped first, which is the supervisor's job and does not
/// exist yet; doing half of it here would leave the console reporting a version it is not
/// running.
pub fn mount_and_start(handle: &Handle, log: &mut dyn FnMut(&str)) {
    let mount = Path::new(paths::PLEX_MOUNT);

    if is_provisioned(mount) {
        log(
            "an app image is already mounted, so the version just installed is not the \
             one running. Nothing stops and restarts Plex yet: reboot the appliance to \
             pick it up.",
        );
        return;
    }

    let outcome = crate::appmount::mount_current(Path::new(paths::PLEX_APPS), mount, log);
    log(&outcome.to_string());
    if matches!(outcome, crate::appmount::Outcome::Refused(_)) {
        return;
    }

    handle.ensure_started(mount, log);
}

/// Prepares the machine and spawns the child that will become Plex.
///
/// # Errors
/// If the preparation fails, or if this executable cannot be found to re-run. The latter
/// means `/usr` is not mounted, which is a far larger problem than Plex not starting.
pub fn start(log: &mut dyn FnMut(&str)) -> io::Result<std::process::Child> {
    let group = prepare(log)?;
    log(&format!("cgroup at {}", group.display()));

    let executable = std::env::current_exe()?;
    std::process::Command::new(executable)
        .arg(CHILD_FLAG)
        .spawn()
}

/// The child side: confine this process and replace it with Plex.
///
/// Never returns on success — the process becomes Plex.
///
/// # Errors
/// Any step of the confinement. None is recoverable, and the caller must not fall back to
/// starting Plex unconfined: a process that meant to confine itself and did not must not
/// go on to run a network-facing media server.
pub fn become_plex(log: &mut dyn FnMut(&str)) -> io::Result<std::convert::Infallible> {
    let mount = Path::new(paths::PLEX_MOUNT);

    // Read here rather than passed in: this process was started with an empty environment
    // by a parent that had one too, so there is nothing to inherit.
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let name = crate::status::os_release_value(&os_release, "NAME").unwrap_or_default();
    let version = crate::status::os_release_value(&os_release, "VERSION_ID").unwrap_or_default();

    // The architecture this binary was built for, which on a single-architecture
    // appliance is the machine it is running on. It goes into the model Plex reports to
    // clients; reading it from `uname` would need a syscall for a string that cannot
    // differ from this one.
    let spec = plexos_plex::run::spec(mount, &name, &version, std::env::consts::ARCH);

    let media = Path::new(paths::MEDIA);
    let libraries = if media.exists() {
        vec![media.to_path_buf()]
    } else {
        // A first boot with no libraries is normal, not broken. run::grants marks media
        // optional for the same reason.
        Vec::new()
    };
    let grants = plexos_plex::run::grants(mount, &libraries);

    let group = Path::new(plexos_plex::cgroup::CGROUP_ROOT).join(plexos_plex::cgroup::PLEX_CGROUP);
    plexos_plex::run::confine_and_exec(&spec, &grants, Some(&group), log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmounted_app_image_is_not_mistaken_for_an_installed_plex() {
        // The mount point exists on every booted machine whether or not anything is
        // mounted on it, so its presence says nothing. Starting a child that then fails
        // to exec would report a confinement error for a machine that simply has no Plex.
        let empty = std::env::temp_dir().join("plexos-plex-empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!is_provisioned(&empty));
        let _ = std::fs::remove_dir(&empty);
    }

    #[test]
    fn a_mounted_app_image_is_recognised_by_the_binary_it_contains() {
        // Including the space in the file name, which is upstream's.
        let root = std::env::temp_dir().join("plexos-plex-mounted");
        let home = root.join(plexos_plex::run::HOME_WITHIN_IMAGE);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(plexos_plex::run::BINARY), b"not really Plex").unwrap();

        assert!(is_provisioned(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_handle_that_has_started_nothing_reports_nothing_running() {
        assert!(!Handle::new().is_running());
    }

    #[test]
    fn an_unprovisioned_machine_is_told_where_plex_would_be_rather_than_failing() {
        // "Plex is not installed" and "Plex would not start" need different responses
        // from whoever reads this. A silent no-op makes them look the same.
        let handle = Handle::new();
        let nowhere = std::env::temp_dir().join("plexos-plex-nothing-here");
        let mut lines = Vec::new();
        handle.ensure_started(&nowhere, &mut |line| lines.push(line.to_owned()));

        let logged = lines.join("\n");
        assert!(logged.contains("not installed"), "{logged}");
        assert!(
            logged.contains("console page"),
            "and names the remedy: {logged}"
        );
        assert!(!handle.is_running());
    }

    #[test]
    fn the_child_flag_is_not_a_flag_anything_else_answers_to() {
        // main() dispatches on it, and a collision would send an ordinary invocation
        // into the confinement path -- which ends in execve and never returns.
        assert!(CHILD_FLAG.starts_with("--"));
        assert_ne!(CHILD_FLAG, "--serve");
        assert_ne!(CHILD_FLAG, "--mount-plex");
    }

    #[test]
    fn the_directories_plex_owns_are_the_ones_the_spec_names() {
        // prepare() and run::spec must agree. If they drift, Plex is handed a data
        // directory it does not own and fails at startup with a permissions error that
        // names neither this list nor that one.
        let spec = plexos_plex::run::spec(
            Path::new(paths::PLEX_MOUNT),
            "PlexOS",
            "0.1.0",
            std::env::consts::ARCH,
        );
        let prepared = [
            PathBuf::from(paths::PLEX_DATA),
            PathBuf::from(paths::PLEX_TRANSCODE_DIR),
        ];
        assert_eq!(spec.owned_directories, prepared);
    }
}
