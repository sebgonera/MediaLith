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
//! # Keeping it running
//!
//! [`supervise`] restarts a Plex that exits. What it watches is deliberately not "is my
//! child alive": a `plexosd` that was itself restarted holds no child while a perfectly
//! good Plex, orphaned onto PID 1, serves away — and the reverse happens too, a child
//! alive and not yet answering during the twenty seconds Plex takes to open its port.
//! Both signals are consulted, and neither alone is trusted.
//!
//! A deliberate stop is not a fault. [`Handle::stop`] records that Plex is *meant* to be
//! down, so the supervisor does not race the shutdown sequence and start a server on a
//! machine that is powering off — which would be the first thing it did, every time.
//!
//! # What has run
//!
//! **This has now started Plex on the appliance**, which then served its own web
//! interface and was claimed to a Plex account. The child's output is captured rather
//! than inherited, and that is why: two failures had to be diagnosed by experiment
//! before it was, and the third was read off the network in one request.

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
    // Created even when empty. Landlock's policy is built from the paths that exist when
    // Plex starts, and become_plex only grants the media root if it is there -- so an
    // appliance whose /var/media appeared later would have a Plex that cannot see its own
    // library, with nothing saying why.
    std::fs::create_dir_all(paths::MEDIA)?;

    // Before Plex starts, and every boot: devtmpfs is assembled fresh each time, so a
    // mode set on a previous boot is not there on this one.
    open_render_nodes_to_plex(log);

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

/// Where DRM devices appear once a driver has bound.
const DRI: &str = "/dev/dri";

/// Makes the GPU's render nodes openable by the account Plex runs as.
///
/// **This is what `udev` does everywhere else, and there is no `udev` here.** DRM does
/// not set a mode on its device nodes, so `devtmpfs` creates them `0600 root:root`; every
/// ordinary distribution then relaxes the render nodes with a rule like
/// `SUBSYSTEM=="drm", KERNEL=="renderD*", MODE="0666"`. PlexOS has no such rule and
/// nothing else was doing it, so Plex — which runs as uid 900 and has its supplementary
/// groups deliberately cleared — could not open the device at all.
///
/// The symptom is the reason this is worth the comment: **every layer above reports
/// success**. `plexos-gpu` says `ready` with the full capability list, because it probes
/// as root. `vainfo` works from a shell, because that is root too. The Landlock grant on
/// `/dev/dri` is present and correct, because Landlock can only restrict what the
/// ordinary permissions already allow — it cannot grant past them. Only Plex fails, and
/// it fails by quietly transcoding on the CPU.
///
/// Render nodes and not `card0`. That distinction is the whole point of render nodes:
/// they carry no modesetting and no access to another client's buffers, which is why
/// they are the node every distribution makes world-accessible and `card0` is the one
/// they do not.
fn open_render_nodes_to_plex(log: &mut dyn FnMut(&str)) {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(entries) = std::fs::read_dir(DRI) else {
        // No /dev/dri at all is a machine with no driver bound, which plexos-gpu reports
        // in its own words. Not this function's business, and not an error here.
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
        {
            continue;
        }

        match std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)) {
            Ok(()) => log(&format!("{} is open to Plex", path.display())),
            Err(error) => log(&format!(
                "could not make {} readable by Plex: {error}. Hardware transcoding will \
                 not work -- Plex runs as uid {} and the node is root-only, which every \
                 report above this will still describe as healthy because they all probe \
                 as root.",
                path.display(),
                paths::PLEX_UID
            )),
        }
    }
}

/// Gives a directory to the Plex account, with a mode only it can use.
fn give_to_plex(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    // std::os::unix::fs::chown, not a syscall of our own: this is one of the few places
    // the standard library already provides what plexos-sys would otherwise have to.
    std::os::unix::fs::chown(path, Some(paths::PLEX_UID), Some(paths::PLEX_GID))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNED_MODE))
}

/// Where Plex listens, on the interface the health gate is allowed to use.
///
/// Loopback, and only loopback. `health`'s documentation forbids any check from
/// depending on the network: Ethernet arrives over USB and enumerates seconds after PCI,
/// so a gate that waited for an address would roll back good updates.
pub const LOOPBACK_ADDRESS: &str = "127.0.0.1:32400";

/// How long a single probe waits before deciding Plex is not there.
///
/// Short: this runs against a process on the same machine, so anything slower than this
/// is Plex being unwell rather than the network being slow.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether Plex is answering HTTP on loopback right now.
///
/// A hand-written request rather than an HTTP client, for the same reason `http` is
/// hand-written: this asks one question of one server on the same machine, and it must
/// work when nothing else does.
///
/// `/identity` is the endpoint asked for because Plex answers it before it has a library,
/// before it is claimed, and without a token. Anything narrower would report a working
/// server as broken on its first boot.
#[must_use]
pub fn is_answering() -> bool {
    use std::io::{Read as _, Write as _};

    let Ok(address) = LOOPBACK_ADDRESS.parse() else {
        return false;
    };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));

    if stream
        .write_all(b"GET /identity HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }

    // Only the status line is needed, and reading the whole body of a server that is
    // misbehaving is how a probe becomes a hang.
    let mut head = [0_u8; 64];
    let Ok(read) = stream.read(&mut head) else {
        return false;
    };
    String::from_utf8_lossy(&head[..read]).contains(" 200 ")
}

/// Waits for Plex to start answering, up to `timeout`.
///
/// Returns whether it did. Plex takes seconds to open its listener — it reads a database
/// and scans its own plugins first — so a gate that probed once immediately after
/// starting it would report a healthy machine as broken every time.
pub fn wait_until_answering(timeout: std::time::Duration, log: &mut dyn FnMut(&str)) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let mut announced = false;

    loop {
        if is_answering() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        if !announced {
            log(&format!(
                "waiting up to {}s for Plex to answer on {LOOPBACK_ADDRESS}",
                timeout.as_secs()
            ));
            announced = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
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
    /// Whether Plex is meant to be running.
    ///
    /// The difference between "Plex is down" and "Plex was stopped", which look identical
    /// from outside and take opposite actions. Without it the supervisor would restart the
    /// server that [`crate::power`] has just stopped in order to turn the machine off.
    wanted: Mutex<bool>,
    /// What the confined child said, bounded.
    ///
    /// An `Arc` because the draining threads outlive the call that started them. Without
    /// this the child's output went only to the console attached to the machine, which
    /// is the one place this project has spent months trying not to need: Plex started
    /// and died on the appliance and the reason was on a screen nobody could read over
    /// the network, so the diagnosis had to be reconstructed by experiment instead.
    log: std::sync::Arc<Mutex<Vec<String>>>,
}

/// Lines kept from the confined child.
///
/// Enough to hold the confinement sequence and Plex's own first complaints, bounded
/// because this lives for the life of the daemon.
pub const MAX_LOG_LINES: usize = 200;

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
                    self.want(true);
                    return;
                }
                Ok(Some(status)) => log(&format!("Plex exited with {status}; starting another")),
                Err(error) => log(&format!("could not tell whether Plex is running: {error}")),
            }
        }

        // Asked of Plex rather than of this process: an earlier invocation may have
        // started it, and starting a second would give two servers fighting over one
        // database.
        if is_answering() {
            log("Plex is already answering on loopback; not starting another");
            // Wanted, even though this process did not start it. A Plex orphaned onto
            // PID 1 by a restarted plexosd is still the Plex this appliance is meant to be
            // running, and if it dies something has to notice.
            self.want(true);
            return;
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
            Ok(mut child) => {
                log(&format!("Plex started as pid {}", child.id()));
                self.want(true);

                // Drained on threads so the child cannot block on a full pipe, and so
                // whatever it says is readable over the network rather than only on the
                // screen attached to the machine.
                if let Some(out) = child.stdout.take() {
                    Self::drain(out, std::sync::Arc::clone(&self.log));
                }
                if let Some(err) = child.stderr.take() {
                    Self::drain(err, std::sync::Arc::clone(&self.log));
                }
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

        // First, and before anything can fail: a stop that is interrupted must still have
        // said that Plex is not wanted, or the supervisor starts another one behind it.
        drop(self.wanted.lock().map(|mut w| *w = false));

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

    /// What the confined child has said so far.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Reads one of the child's streams into [`Self::log`] until it closes.
    fn drain(stream: impl std::io::Read + Send + 'static, log: std::sync::Arc<Mutex<Vec<String>>>) {
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::BufReader::new(stream)
                .lines()
                .map_while(Result::ok)
            {
                // Still printed: the console attached to the machine is the only thing
                // that works when the network does not.
                println!("plexosd: plex: {line}");
                let mut held = log
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if held.len() >= MAX_LOG_LINES {
                    held.remove(0);
                }
                held.push(line);
            }
        });
    }

    /// Records whether Plex is meant to be running.
    fn want(&self, wanted: bool) {
        *self
            .wanted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = wanted;
    }

    /// Whether Plex is meant to be running.
    #[must_use]
    pub fn is_wanted(&self) -> bool {
        *self
            .wanted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether this process holds a child that has not exited.
    ///
    /// Distinct from [`Handle::is_running`], which asks Plex. A child can be alive and not
    /// yet answering — Plex takes about twenty seconds to open its port — and a Plex can be
    /// answering while this process holds nothing, which is what a restarted `plexosd`
    /// finds.
    #[must_use]
    pub fn holds_live_child(&self) -> bool {
        let mut held = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// Whether Plex is up.
    ///
    /// Answers the question by asking Plex, not by asking whether this process holds a
    /// live child. The two differ in both directions and the difference matters: a child
    /// can be alive and wedged, and a Plex started by an earlier `plexosd` invocation is
    /// running perfectly well while this one owns nothing.
    #[must_use]
    pub fn is_running(&self) -> bool {
        let _ = self;
        is_answering()
    }
}

/// Mounts a freshly-provisioned app image and starts Plex from it.
///
/// The path a first installation takes: at boot there was nothing to mount, so
/// `--mount-plex` did nothing, and without this the administrator would have to reboot
/// the appliance to use the Plex they just installed.
///
/// An *already mounted* image means an upgrade rather than a first install, and is handed
/// to [`swap`].
pub fn mount_and_start(handle: &Handle, log: &mut dyn FnMut(&str)) {
    let mount = Path::new(paths::PLEX_MOUNT);

    if is_provisioned(mount) {
        swap(handle, log);
        return;
    }

    let outcome = crate::appmount::mount_current(Path::new(paths::PLEX_APPS), mount, log);
    log(&outcome.to_string());
    if matches!(outcome, crate::appmount::Outcome::Refused(_)) {
        return;
    }

    handle.ensure_started(mount, log);
}

/// How long Plex is given to exit before an upgrade gives up on it.
///
/// Longer than the shutdown sequence allows, and for the same reason it exists at all:
/// Plex keeps its library in `SQLite`, and being killed mid-write is the damage this is
/// avoiding. An upgrade is not urgent, so it can afford to wait.
pub const SWAP_GRACE: std::time::Duration = std::time::Duration::from_secs(45);

/// Replaces a running Plex with the version that was just installed.
///
/// Stop, unmount, mount what `current` now points at, start. The order is the whole of it:
/// the image cannot be unmounted while Plex holds files open on it, and mounting the new
/// one over the old would leave the console reporting a version that is not running.
///
/// Reports each step, because this is minutes of a machine deliberately serving nothing
/// and a person watching a page needs to see it is not stuck.
///
/// # What this does not do
///
/// It does not put the old version back if the new one fails to start. The app images are
/// both still on `/var` and `current` still points at the new one, so the remedy is a
/// reboot or another install — not a rollback this code performs badly under pressure.
/// ADR-0005 covers the operating system; nothing covers Plex, and pretending otherwise
/// here would be the third design in this repository that was complete, tested and wrong.
pub fn swap(handle: &Handle, log: &mut dyn FnMut(&str)) {
    let mount = Path::new(paths::PLEX_MOUNT);

    log("a new version is installed; stopping the running Plex to swap it in");
    handle.stop(SWAP_GRACE, log);

    let removal = crate::appmount::unmount_current(mount, log);
    log(&removal.to_string());
    if matches!(removal, crate::appmount::Removal::Failed(_)) {
        // Deliberately started again. The unmount failed, so the *old* image is still
        // mounted and still whole; leaving Plex stopped would turn a failed upgrade into
        // an appliance with no media server, which is strictly worse than an appliance
        // running last week's version.
        log("putting the running version back, since nothing was replaced");
        handle.ensure_started(mount, log);
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
        // Piped rather than inherited, so the confinement log and Plex's own first
        // complaints can be read over the network. Handle::ensure_started drains both;
        // leaving them piped and unread would eventually block the child on a full pipe.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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

/// How often the supervisor looks.
///
/// Five seconds. Plex takes about twenty to start answering, so a shorter interval would
/// only ask the same question more often, and a much longer one is time an appliance
/// spends serving nothing while somebody refreshes a page.
pub const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Delay before each successive restart of Plex.
///
/// Longer than PID 1's, because starting Plex is not cheap — it mounts nothing but it
/// forks, builds a Landlock policy and opens a database — and because a Plex that cannot
/// start will not start on the fourth attempt either. The last figure is five minutes: an
/// appliance that has given up trying quickly is still trying, and somebody who fixes the
/// cause does not have to know that.
pub const RESTART_BACKOFF: &[std::time::Duration] = &[
    std::time::Duration::from_secs(0),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(15),
    std::time::Duration::from_secs(60),
    std::time::Duration::from_secs(300),
];

/// What the supervisor can see of the machine.
///
/// Only observations. Whether Plex is *meant* to be running is a separate argument to
/// [`restart_reason`], and separate because it is a different kind of fact: everything here
/// is read off the machine, and that is read off an intention somebody expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    /// Whether this process holds a child that has not exited.
    pub holds_live_child: bool,
    /// Whether something is answering Plex's port on loopback.
    pub answering: bool,
    /// Whether there is an app image to start.
    pub provisioned: bool,
}

/// Why Plex should be started again, or `None` if it should not.
///
/// Every branch that declines is a case that looks like a dead Plex and is not, and each
/// one would produce a different wrong action:
///
/// - **not wanted**: the shutdown sequence has stopped it. Starting another here is a
///   server brought up on a machine that is powering off, every time.
/// - **a live child**: Plex is starting. It takes about twenty seconds to answer, and a
///   supervisor that watched only the port would start a second one into the same
///   database during that window.
/// - **answering**: something is serving, and it is not this process's child — a Plex
///   orphaned onto PID 1 when `plexosd` was restarted. It is doing its job.
/// - **not provisioned**: there is nothing installed to start, which is a state to report
///   once rather than to retry for ever.
#[must_use]
pub fn restart_reason(wanted: bool, seen: &Presence) -> Option<&'static str> {
    if !wanted {
        return None;
    }
    if seen.holds_live_child {
        return None;
    }
    if seen.answering {
        return None;
    }
    if !seen.provisioned {
        return None;
    }
    Some("Plex is not running and is not answering")
}

/// Restarts Plex when it exits, for the life of the daemon.
///
/// Never returns. Spawned on a thread by the console, because the console has to stay
/// usable on precisely the machine where Plex will not start.
pub fn supervise(handle: &Handle, mount: &Path, log: &mut dyn FnMut(&str)) -> ! {
    let mut failures = 0usize;
    let mut next_attempt = std::time::Instant::now();

    loop {
        std::thread::sleep(WATCH_INTERVAL);

        let seen = Presence {
            holds_live_child: handle.holds_live_child(),
            answering: is_answering(),
            provisioned: is_provisioned(mount),
        };

        if restart_reason(handle.is_wanted(), &seen).is_none() {
            // Healthy, or deliberately down. Either way the history is forgiven: the next
            // failure should be treated as the first one, not as the fifth.
            failures = 0;
            continue;
        }

        if std::time::Instant::now() < next_attempt {
            continue;
        }

        let delay = RESTART_BACKOFF[failures.min(RESTART_BACKOFF.len() - 1)];
        log(&format!(
            "Plex is not running; starting it again{}",
            if delay.is_zero() {
                String::new()
            } else {
                format!(
                    " (attempt {}, then waiting {}s)",
                    failures + 1,
                    delay.as_secs()
                )
            }
        ));
        handle.ensure_started(mount, log);

        failures = failures.saturating_add(1);
        next_attempt =
            std::time::Instant::now() + RESTART_BACKOFF[failures.min(RESTART_BACKOFF.len() - 1)];
    }
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

    /// What a healthy, running Plex looks like from outside.
    fn healthy() -> Presence {
        Presence {
            holds_live_child: true,
            answering: true,
            provisioned: true,
        }
    }

    #[test]
    fn a_plex_that_exited_is_started_again() {
        // The gap this closes. A Plex that died stayed dead on a machine with no keyboard
        // anybody is expected to use, so the remedy was a reboot.
        let dead = Presence {
            holds_live_child: false,
            answering: false,
            ..healthy()
        };
        assert!(restart_reason(true, &dead).is_some());
    }

    #[test]
    fn a_deliberate_stop_is_not_a_fault() {
        // Otherwise the first thing the supervisor does during a shutdown is start a
        // server on a machine that is powering off, every single time.
        let dead = Presence {
            holds_live_child: false,
            answering: false,
            ..healthy()
        };
        assert_eq!(restart_reason(false, &dead), None);
    }

    #[test]
    fn a_plex_that_is_still_starting_is_left_alone() {
        // Plex takes about twenty seconds to open its port. A supervisor watching only the
        // port would start a second server into the same database inside that window,
        // which is the one failure worse than no supervisor at all.
        let starting = Presence {
            holds_live_child: true,
            answering: false,
            ..healthy()
        };
        assert_eq!(restart_reason(true, &starting), None);
    }

    #[test]
    fn a_plex_this_process_does_not_own_is_left_alone() {
        // What a restarted plexosd finds: its predecessor's Plex, orphaned onto PID 1 and
        // serving perfectly well. Owning nothing is not the same as nothing running, and
        // this is exactly why is_running asks Plex rather than asking the handle.
        let orphaned = Presence {
            holds_live_child: false,
            answering: true,
            ..healthy()
        };
        assert_eq!(restart_reason(true, &orphaned), None);
    }

    #[test]
    fn an_appliance_with_no_plex_installed_is_not_a_restart_loop() {
        // Every appliance is in this state until somebody installs Plex from the console.
        let empty = Presence {
            holds_live_child: false,
            answering: false,
            provisioned: false,
        };
        assert_eq!(restart_reason(true, &empty), None);
    }

    #[test]
    fn the_restart_delays_are_ordered_and_end_somewhere_a_person_can_wait() {
        assert!(RESTART_BACKOFF.windows(2).all(|w| w[0] <= w[1]));
        assert!(RESTART_BACKOFF[0].is_zero(), "the first retry is immediate");
        assert!(
            *RESTART_BACKOFF.last().unwrap() <= std::time::Duration::from_secs(600),
            "an appliance that has given up trying quickly must still be trying, so that \
             fixing the cause does not also require knowing to reboot"
        );
        assert!(
            WATCH_INTERVAL <= RESTART_BACKOFF[1],
            "a watch slower than the first backoff makes the backoff decorative: the \
             interval would be doing the waiting"
        );
    }

    #[test]
    fn a_fresh_handle_does_not_want_plex_running() {
        // Nothing has asked for it yet, and a supervisor that assumed otherwise would try
        // to start Plex on an appliance where none is installed, before the console has
        // even said so.
        let handle = Handle::new();
        assert!(!handle.is_wanted());
        assert!(!handle.holds_live_child());
    }

    #[test]
    fn stopping_records_that_plex_is_not_wanted_even_with_nothing_to_stop() {
        // The order matters: `stop` returns early when it holds no child, and the flag has
        // to have been written before that -- otherwise a shutdown on a machine whose Plex
        // was started by an earlier plexosd leaves the supervisor free to start another.
        let handle = Handle::new();
        let mut log = |_: &str| {};
        handle.stop(std::time::Duration::from_millis(1), &mut log);
        assert!(!handle.is_wanted());
    }
}
