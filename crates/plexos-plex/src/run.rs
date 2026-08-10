//! Starting Plex, confined.
//!
//! The environment comes from Plex's own `plexmediaserver.service`, read out of the
//! package rather than invented: `PLEX_MEDIA_SERVER_HOME`,
//! `PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR` and the informational `_INFO_*` trio are
//! what upstream sets, and guessing them produces a Plex that starts and then behaves
//! oddly in ways nothing connects back to a missing variable.
//!
//! # Why this re-executes `plexosd` instead of using `pre_exec`
//!
//! The confinement has to apply to the child and to nothing else. The obvious tool is
//! `CommandExt::pre_exec`, and it is `unsafe` — in a crate that forbids `unsafe`, for
//! the reason ADR-0011 gives. Moving the closure into `plexos-sys` would mean putting
//! process orchestration in the syscall layer to borrow its exemption.
//!
//! So the child is a fresh `plexosd` invoked with a flag, and it confines *itself*
//! before `exec`ing Plex: join the cgroup, apply Landlock, drop privileges, exec. Every
//! step is an ordinary call in an ordinary process, and the whole sequence can be run
//! by hand from a shell when it misbehaves — which `pre_exec` could never be.
//!
//! # What is verified and what is not
//!
//! The spec and the ordering are tested, and **Plex has now been started by this on the
//! appliance**: cgroup joined, Landlock applied, privileges dropped to 900:900, and
//! `execve` into Plex, which then served and was claimed to a Plex account.
//!
//! Getting there cost two corrections to the grant list, both of the same shape and both
//! recorded as traps: a deny-by-default policy has to be *executed* before it can be
//! believed, because the paths a process needs are mostly ones nobody thinks to list.

use std::path::{Path, PathBuf};

use plexos_types::paths;

/// Where Plex's own files sit inside the mounted app image.
pub const HOME_WITHIN_IMAGE: &str = "usr/lib/plexmediaserver";

/// The binary, relative to [`HOME_WITHIN_IMAGE`]. The space is upstream's.
pub const BINARY: &str = "Plex Media Server";

/// Upstream's cap on plugin processes, from `plexmediaserver.service`.
const MAX_PLUGIN_PROCS: &str = "6";

/// Everything needed to start Plex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Absolute path of the executable.
    pub binary: PathBuf,
    /// `NAME=value` pairs, replacing the environment rather than adding to it.
    pub environment: Vec<(String, String)>,
    /// Directories that must exist and be owned by Plex before it starts.
    pub owned_directories: Vec<PathBuf>,
}

/// Builds the spec for an app image mounted at `mount`.
///
/// `os_name` and `os_version` go into the `_INFO_` variables Plex reports to clients;
/// upstream's unit derives them from `/etc/os-release` and so do we, rather than
/// leaving them empty and having the server describe itself as nothing.
#[must_use]
pub fn spec(mount: &Path, os_name: &str, os_version: &str, machine: &str) -> Spec {
    let home = mount.join(HOME_WITHIN_IMAGE);

    Spec {
        binary: home.join(BINARY),
        environment: vec![
            (
                "PLEX_MEDIA_SERVER_HOME".to_owned(),
                home.display().to_string(),
            ),
            (
                "PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR".to_owned(),
                paths::PLEX_DATA.to_owned(),
            ),
            (
                "PLEX_MEDIA_SERVER_MAX_PLUGIN_PROCS".to_owned(),
                MAX_PLUGIN_PROCS.to_owned(),
            ),
            // Transcoding writes here. Without it Plex uses /tmp, which on this
            // appliance is a tmpfs -- so a 4K transcode would be written into RAM and
            // the machine would meet the OOM killer instead of finishing the file.
            ("TMPDIR".to_owned(), paths::PLEX_TRANSCODE_DIR.to_owned()),
            (
                "PLEX_MEDIA_SERVER_INFO_VENDOR".to_owned(),
                os_name.to_owned(),
            ),
            (
                "PLEX_MEDIA_SERVER_INFO_MODEL".to_owned(),
                machine.to_owned(),
            ),
            (
                "PLEX_MEDIA_SERVER_INFO_PLATFORM_VERSION".to_owned(),
                os_version.to_owned(),
            ),
            // Absolute, and set explicitly because the parent has none: plexosd is
            // started by PID 1 and inherits an empty environment. Plex's own launcher
            // shells out to grep, awk and uname.
            ("PATH".to_owned(), crate::tools::PROGRAM_DIRS.join(":")),
        ],
        owned_directories: vec![
            PathBuf::from(paths::PLEX_DATA),
            PathBuf::from(paths::PLEX_TRANSCODE_DIR),
        ],
    }
}

/// The device paths libcuda names, read out of the library rather than listed from
/// memory: `strings libcuda.so | grep '^/dev/'`.
///
/// These four are character devices. `/dev/nvidia-caps` is a directory and is
/// [`NVIDIA_CAPS`], separately, because Landlock validates access bits against what it is
/// granting and the two need different ones.
///
/// `/dev/char/<major>:<minor>` is named by `libcuda` too and is deliberately absent: a
/// `udev` artefact, missing on this system, and missing in the unconfined run that
/// worked, so it is not what was being denied.
pub const NVIDIA_NODES: [&str; 4] = [
    "/dev/nvidiactl",
    "/dev/nvidia0",
    "/dev/nvidia-uvm",
    "/dev/nvidia-uvm-tools",
];

/// The capability directory, kept apart from the nodes because Landlock treats the two
/// differently and mixing them cost a boot.
///
/// It is also not there when Plex starts unless something has already woken the driver:
/// what is inside it appears when the GPU is first initialised, not when the module
/// loads. A rule cannot be added for a path that does not exist, so `plexos-init` opens
/// `/dev/nvidiactl` once to force that initialisation before any service runs.
pub const NVIDIA_CAPS: &str = "/dev/nvidia-caps";

/// A path Plex may reach, and what it may do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The directory.
    pub path: PathBuf,
    /// Landlock access bits.
    pub access: u64,
    /// Whether Plex cannot start without it.
    pub required: bool,
}

/// The filesystem policy: everything Plex may touch, and nothing else.
///
/// ADR-0007 lists four things. They are spelled out here rather than assembled at the
/// call site so that the policy is one readable list, and so a test can assert that
/// nothing writable is also executable.
#[must_use]
pub fn grants(mount: &Path, media: &[PathBuf]) -> Vec<Grant> {
    use plexos_sys::landlock::access;

    let mut grants = vec![
        // The base system, read and execute. This is not a relaxation of ADR-0007 bolted
        // on afterwards; it is what the ADR's four paths turned out to be missing. A
        // ruleset that handles every filesystem operation denies everything it does not
        // grant, including the dynamic loader opening libc -- so with the original list
        // Plex could not be executed at all, and did not survive its own execve. That
        // was measured rather than reasoned about: applying this policy and then running
        // /bin/echo fails with EACCES, and /proc/self/status is unreadable.
        //
        // Granting it costs little. /usr is a read-only dm-verity mount, so read and
        // execute there is exactly what every process on this machine already has and
        // nothing Plex could change. What matters is what is still *not* granted, below.
        Grant {
            path: PathBuf::from(paths::USR),
            access: access::READ_EXECUTE,
            required: true,
        },
        // Configuration: resolv.conf for DNS, localtime for timestamps, and passwd --
        // without which getpwuid(900) fails and Plex cannot learn its own account.
        Grant {
            path: PathBuf::from("/etc"),
            access: access::READ_ONLY,
            required: true,
        },
        // Plex reads /proc/self, /proc/cpuinfo and /proc/meminfo to size its own work.
        Grant {
            path: PathBuf::from("/proc"),
            access: access::READ_ONLY,
            required: true,
        },
        // /dev/null and /dev/urandom, which any non-trivial process opens. Write is
        // included because /dev/null is written to; this is a devtmpfs with the standard
        // node set and nothing on it is persistent state.
        Grant {
            path: PathBuf::from("/dev"),
            access: access::READ_FILE | access::WRITE_FILE | access::READ_DIR,
            required: true,
        },
        // /run, read-only, and this one is not obvious. /etc/resolv.conf is a symlink to
        // ../run/resolv.conf -- Buildroot's skeleton makes it one so that a read-only
        // /etc can still have a lease-managed resolver. Landlock resolves symlinks, so a
        // rule on /etc does not cover the target, and without this Plex cannot read the
        // file at all. musl does not report that as an error: it falls back to
        // 127.0.0.1, where nothing listens, and every lookup fails with "Could not
        // resolve host". That is what stopped the server being claimed, and it looked
        // like a network fault on a machine whose network was fine.
        //
        // The directory rather than the file, deliberately. udhcpc rewrites resolv.conf
        // on every lease renewal, and a rule tied to the old file would stop covering
        // the new one -- so DNS would work until the first renewal and then stop, which
        // is a far worse bug than this one.
        Grant {
            path: PathBuf::from("/run"),
            access: access::READ_ONLY,
            required: true,
        },
        // Hardware discovery. Not required: a machine whose sysfs is unreadable is
        // broken in ways that are not Plex's problem, and the transcoder falls back to
        // software rather than failing.
        Grant {
            path: PathBuf::from("/sys"),
            access: access::READ_ONLY,
            required: false,
        },
        // The app image. Read and execute, never write: it is a read-only erofs mount
        // and saying so here means a future writable one does not silently become
        // writable to Plex.
        Grant {
            path: mount.to_path_buf(),
            access: access::READ_EXECUTE,
            required: true,
        },
        // Its own data. The media database lives here -- and so do the codecs Plex
        // downloads for itself, which is why this grants EXECUTE as well.
        //
        // That was not obvious and it cost a working film. Plex does not ship every audio
        // encoder: EAC3, TrueHD and DTS go through EasyAudioEncoder, which Plex fetches at
        // runtime into `Codecs/` under this directory and then runs as a separate process.
        // With READ_WRITE alone the binary downloads fine, is mode 0755, runs perfectly
        // from a shell -- and never starts under the policy. What the user sees is
        // "EasyAudioEncoder failed", and what the log says is "EAE not running, or wrong
        // folder?", which points at a folder that is right.
        //
        // Third instance of one shape, after `/usr` and `/run`: a deny-by-default policy
        // missing something nobody had listed, discovered only when a file that needed it
        // was finally played.
        //
        // # What this costs, stated plainly
        //
        // Write and execute on the same directory means Plex can run anything it can
        // write there. The narrower grant -- execute only on `Codecs/` -- was considered
        // and rejected twice over: Landlock rules need the path to exist when the ruleset
        // is built, so a machine that has not downloaded a codec yet would silently get no
        // rule and fail exactly as before until a restart; and creating that directory
        // ourselves would mean PlexOS writing into a layout ADR-0010 says belongs to Plex.
        //
        // What the confinement is *for* is unchanged: nothing outside these grants is
        // reachable, so `/etc`, `/root`, the ESP, the update path and every other slot
        // remain closed. This widens what Plex may do inside its own data directory, which
        // is a place Plex already controls completely.
        Grant {
            path: PathBuf::from(paths::PLEX_DATA),
            access: access::READ_WRITE | access::EXECUTE,
            required: true,
        },
        // Transcode scratch.
        Grant {
            path: PathBuf::from(paths::PLEX_TRANSCODE_DIR),
            access: access::READ_WRITE,
            required: true,
        },
        // The GPU. IOCTL_DEV is the whole point -- VA-API drives the render node
        // through ioctls, and without this bit hardware transcoding fails in a way
        // that looks like a missing driver.
        Grant {
            path: PathBuf::from("/dev/dri"),
            access: access::READ_FILE | access::WRITE_FILE | access::READ_DIR | access::IOCTL_DEV,
            required: false,
        },
        // The other GPU, and it is a different set of paths entirely. NVIDIA does not
        // use `/dev/dri` for decode and encode -- it has its own nodes, made by
        // plexos-init because devtmpfs will not, and reached by ioctl exactly as the
        // render node is.
        //
        // These are named one by one rather than granting `/dev`, which would hand Plex
        // every device on the machine to save four lines.
        //
        // `required: false` because most machines have no NVIDIA card and a missing path
        // must not stop Plex starting. That is the same reasoning as `/dev/dri` and it
        // has the same cost: if these are absent on a machine that does have a card,
        // Plex starts and transcodes on the CPU, and the only place that says so is the
        // GPU report. Fourth time this deny-by-default policy has been missing something
        // nobody listed -- after /usr, /run, and the audio encoder's execute bit -- so it
        // is worth saying plainly that the list is the thing that goes stale.
    ];

    // Device *files*. READ_DIR must not appear here: Landlock validates access bits
    // against what it is granting, and a directory right on a character device is
    // rejected with EINVAL. That does not fail loudly -- the rule is `skipped`, the
    // policy applies without it, and Plex loses the device it had. Adding READ_DIR here
    // while fixing something else did exactly that to four working grants, and the
    // confinement log was the only place it showed.
    for node in NVIDIA_NODES {
        grants.push(Grant {
            path: PathBuf::from(node),
            access: access::READ_FILE | access::WRITE_FILE | access::IOCTL_DEV,
            required: false,
        });
    }

    // And the directory, which takes the right the files must not have.
    grants.push(Grant {
        path: PathBuf::from(NVIDIA_CAPS),
        access: access::READ_FILE | access::READ_DIR | access::IOCTL_DEV,
        required: false,
    });

    // Media, read-only. Not required: a server with no libraries configured yet is a
    // normal first-boot state, not a broken one.
    grants.extend(media.iter().map(|path| Grant {
        path: path.clone(),
        access: access::READ_ONLY,
        required: false,
    }));

    grants
}

/// Confines this process and replaces it with Plex.
///
/// Runs in the child, which is a fresh `plexosd` started for the purpose — see the
/// module documentation for why that is a re-exec rather than a `pre_exec` closure.
///
/// The order is the whole of it, and each step is where it is because the next one
/// takes away the ability to do it:
///
/// 1. **Join the cgroup.** Writing to `cgroup.procs` needs privilege this still has.
///    Done first so there is no instant in which Plex runs outside its bounds.
/// 2. **Apply Landlock.** Before dropping privileges, because the paths being granted
///    have to be opened, and some of them are root-owned.
/// 3. **Drop to the Plex account.** Irreversible.
/// 4. **`exec`.** Landlock and `no_new_privs` are inherited across it; that is the
///    property that makes this worth doing at all.
///
/// # Errors
/// Any step failing. None is recoverable: a process that meant to confine itself and
/// did not must not go on to run a network-facing media server.
pub fn confine_and_exec(
    spec: &Spec,
    grants: &[Grant],
    cgroup: Option<&Path>,
    log: &mut dyn FnMut(&str),
) -> std::io::Result<std::convert::Infallible> {
    use plexos_sys::landlock::Ruleset;

    if let Some(group) = cgroup {
        crate::cgroup::join(group, std::process::id())?;
        log(&format!("joined {}", group.display()));
    }

    let mut ruleset = Ruleset::new(plexos_sys::landlock::access::ALL)?;
    for grant in grants {
        match ruleset.allow(&grant.path, grant.access) {
            Ok(()) => log(&format!("granted {}", grant.path.display())),
            // A missing media directory is a library nobody has created yet, and
            // refusing to start Plex over it would make an ordinary misconfiguration
            // look like a broken appliance. A missing *required* path is different:
            // Plex cannot work without it and starting anyway only moves the failure.
            Err(error) if !grant.required => {
                log(&format!("skipped {}: {error}", grant.path.display()));
            }
            Err(error) => return Err(error),
        }
    }
    ruleset.enforce()?;
    log("Landlock applied; no path outside those grants is reachable from here");

    plexos_sys::privilege::drop_to(paths::PLEX_UID, paths::PLEX_GID)?;
    log(&format!(
        "running as {}:{}",
        paths::PLEX_UID,
        paths::PLEX_GID
    ));

    let environment: Vec<(&str, &str)> = spec
        .environment
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    plexos_sys::process::exec_with_env(&spec.binary.to_string_lossy(), &[], &environment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexos_sys::landlock::access;

    fn mount() -> PathBuf {
        PathBuf::from(paths::PLEX_MOUNT)
    }

    fn value<'a>(spec: &'a Spec, key: &str) -> Option<&'a str> {
        spec.environment
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn plex_may_execute_the_codecs_it_downloads_into_its_own_data_directory() {
        // Plex does not ship every audio encoder. EAC3, TrueHD and DTS go through
        // EasyAudioEncoder, which it fetches at runtime into Codecs/ under PLEX_DATA and
        // then runs. Granted read and write but not execute, the download succeeds, the
        // file is 0755, it runs from a shell -- and never starts under the policy. The
        // symptom is "EasyAudioEncoder failed" and a log line blaming the folder.
        let grants = grants(&mount(), &[]);
        let data = grants
            .iter()
            .find(|g| g.path == std::path::Path::new(paths::PLEX_DATA))
            .expect("Plex's data directory is always granted");

        assert!(
            data.access & access::EXECUTE != 0,
            "a codec Plex downloads here has to be runnable, or the format needing it \
             fails with an error that points at the wrong thing"
        );
        assert!(
            data.access & access::WRITE_FILE != 0,
            "and it still has to be able to download it"
        );
    }

    #[test]
    fn the_binary_is_where_plexs_own_package_puts_it() {
        // Including the space in the file name, which is upstream's and not a typo.
        let spec = spec(&mount(), "PlexOS", "0.1.0", "x86_64");
        assert_eq!(
            spec.binary,
            Path::new("/run/plexos/plex/usr/lib/plexmediaserver/Plex Media Server")
        );
    }

    #[test]
    fn the_environment_matches_plexs_own_unit_file() {
        // Read out of plexmediaserver.service in the package rather than invented. A
        // missing variable here does not stop Plex starting; it makes it behave oddly
        // later in ways nothing connects back to this list.
        let spec = spec(&mount(), "PlexOS", "0.1.0", "x86_64");
        assert_eq!(
            value(&spec, "PLEX_MEDIA_SERVER_HOME"),
            Some("/run/plexos/plex/usr/lib/plexmediaserver")
        );
        assert_eq!(
            value(&spec, "PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR"),
            Some(paths::PLEX_DATA)
        );
        assert_eq!(
            value(&spec, "PLEX_MEDIA_SERVER_MAX_PLUGIN_PROCS"),
            Some("6")
        );
        assert_eq!(
            value(&spec, "PLEX_MEDIA_SERVER_INFO_VENDOR"),
            Some("PlexOS")
        );
    }

    #[test]
    fn transcoding_does_not_go_to_tmpfs() {
        // Without TMPDIR Plex uses /tmp, which here is a tmpfs. A 4K transcode would
        // then be written into RAM, and the machine would meet the OOM killer rather
        // than finish the file.
        let spec = spec(&mount(), "PlexOS", "0.1.0", "x86_64");
        assert_eq!(value(&spec, "TMPDIR"), Some(paths::PLEX_TRANSCODE_DIR));
        assert!(
            paths::PLEX_TRANSCODE_DIR.starts_with("/var/"),
            "and it is on disk"
        );
    }

    #[test]
    fn a_path_is_provided_because_the_parent_has_none() {
        // plexosd is started by PID 1 and inherits an empty environment; Plex's own
        // launcher shells out to grep, awk and uname. This is the same trap that cost
        // a boot in plexosd::net.
        let spec = spec(&mount(), "PlexOS", "0.1.0", "x86_64");
        let path = value(&spec, "PATH").expect("a PATH");
        assert!(path.contains("/usr/bin"), "{path}");
        assert!(path.contains("/sbin"), "{path}");
    }

    #[test]
    fn the_app_image_is_executable_and_never_writable() {
        // It is a read-only mount today. Saying so in the policy means that if it ever
        // stops being one, Plex still cannot write to it.
        let grants = grants(&mount(), &[]);
        let image = grants.iter().find(|g| g.path == mount()).unwrap();
        assert_ne!(image.access & access::EXECUTE, 0);
        assert_eq!(image.access & access::WRITE_FILE, 0);
    }

    #[test]
    fn the_policy_can_actually_execute_a_dynamically_linked_program() {
        // The property the original four-path policy lacked, and the reason Plex started
        // and died in the same instant on the appliance. A ruleset that handles every
        // filesystem operation denies what it does not grant, so without /usr the
        // dynamic loader cannot open libc and execve fails before Plex runs a line.
        //
        // Asserted structurally here; proved by execution in examples/landlock-demo and,
        // when it was broken, by applying this exact set and watching /bin/echo fail with
        // EACCES.
        let grants = grants(&mount(), &[]);
        let find = |p: &str| {
            grants
                .iter()
                .find(|g| g.path == Path::new(p))
                .unwrap_or_else(|| panic!("{p} must be granted or nothing can run"))
        };

        let usr = find(paths::USR);
        assert_ne!(usr.access & access::EXECUTE, 0, "libc must be executable");
        assert_ne!(usr.access & access::READ_FILE, 0);
        assert!(usr.required, "nothing runs without it");

        // getpwuid(900) reads /etc/passwd; without it Plex cannot learn its own account.
        assert_ne!(find("/etc").access & access::READ_FILE, 0);
        assert_ne!(find("/proc").access & access::READ_FILE, 0);
        // /dev/null is opened by almost everything, and written to.
        assert_ne!(find("/dev").access & access::WRITE_FILE, 0);
    }

    #[test]
    fn the_resolver_configuration_is_reachable_through_its_symlink() {
        // /etc/resolv.conf is a symlink to ../run/resolv.conf, and Landlock resolves
        // symlinks -- so granting /etc does not grant the target. Without /run, musl
        // falls back to 127.0.0.1 without saying so and every DNS lookup fails. Plex
        // could not be claimed because of exactly this.
        let grants = grants(&mount(), &[]);
        let run = grants
            .iter()
            .find(|g| g.path == Path::new("/run"))
            .expect("/run must be granted or Plex has no DNS");
        assert_ne!(run.access & access::READ_FILE, 0);
        assert_eq!(run.access & access::WRITE_FILE, 0, "read-only is enough");
        assert!(
            run.required,
            "a Plex that cannot resolve a name is not working"
        );
    }

    #[test]
    fn the_base_system_is_readable_and_never_writable() {
        // What granting /usr, /etc, /proc and /sys must not cost: Plex may read the
        // system it runs on and may not change any of it.
        for path in [paths::USR, "/etc", "/proc", "/sys", "/run"] {
            let grant = grants(&mount(), &[])
                .into_iter()
                .find(|g| g.path == Path::new(path))
                .expect("granted");
            assert_eq!(
                grant.access & (access::WRITE_FILE | access::MAKE_REG | access::REMOVE_FILE),
                0,
                "{path} must not be writable by Plex"
            );
        }
    }

    #[test]
    fn plexoss_own_state_is_not_reachable_from_plex() {
        // The device token lives in /var/lib/plexos, and the media database in
        // /var/lib/plex. Granting the second must never grant the first: a compromised
        // Plex that could read the token could install whatever it liked over the
        // console. Nothing here may name /var itself, and the granted paths must sit
        // strictly below it.
        let state = Path::new(plexos_types::paths::PLEXOS_STATE);
        for grant in grants(&mount(), &[]) {
            assert_ne!(
                grant.path,
                Path::new(paths::VAR),
                "granting /var grants the token"
            );
            assert!(
                !state.starts_with(&grant.path) || grant.path == Path::new("/"),
                "{} would make {} reachable",
                grant.path.display(),
                state.display()
            );
        }
    }

    #[test]
    fn nothing_writable_is_also_executable_except_where_plex_keeps_its_codecs() {
        // The property worth having: a directory Plex can write to is a directory an
        // exploit can drop a binary into, and one it can also execute from is a complete
        // escape. Media is read-only; the transcode scratch and the app image keep it.
        //
        // PLEX_DATA is the one exception, and it is deliberate rather than an oversight
        // that outgrew a test. Plex does not ship EAC3, TrueHD or DTS encoders: it
        // downloads EasyAudioEncoder into Codecs/ under that directory at runtime and
        // runs it. Denying execute there does not prevent the download, it prevents
        // playback of those formats, with an error that names the encoder and a log line
        // blaming a folder that is correct.
        //
        // The exception is named here so that a future grant cannot acquire write and
        // execute quietly. Anything else in this list gaining both fails this test.
        let exception = std::path::Path::new(paths::PLEX_DATA);
        let mut found_the_exception = false;

        for grant in grants(&mount(), &[PathBuf::from("/var/media/films")]) {
            let writable = grant.access & access::WRITE_FILE != 0;
            let executable = grant.access & access::EXECUTE != 0;

            if grant.path == exception {
                found_the_exception = true;
                continue;
            }

            assert!(
                !(writable && executable),
                "{} is both writable and executable",
                grant.path.display()
            );
        }

        assert!(
            found_the_exception,
            "the exception must still be in the grant list, or this test is exempting \
             something that is not there and checking nothing"
        );
    }

    #[test]
    fn the_nvidia_nodes_are_granted_ioctl_too() {
        // /dev is already granted read and write, which is why this is easy to think is
        // already handled -- and it is not, because the bit that matters is IOCTL_DEV.
        // NVDEC and NVENC are driven by ioctl on /dev/nvidiactl and /dev/nvidia0 exactly
        // as VA-API drives the render node, and without it Plex opens the device, gets
        // EACCES on the first ioctl, and falls back to the CPU.
        //
        // Fourth time this policy has been missing something nobody listed, after /usr,
        // /run and the audio encoder's execute bit. The list is the thing that goes
        // stale, so this asserts every node rather than a representative one.
        let grants = grants(&mount(), &[]);
        for node in NVIDIA_NODES.iter().chain(std::iter::once(&NVIDIA_CAPS)) {
            let grant = grants
                .iter()
                .find(|g| g.path == Path::new(*node))
                .unwrap_or_else(|| panic!("{node} is not in the policy at all"));
            assert!(
                grant.access & access::IOCTL_DEV != 0,
                "{node} is granted without IOCTL_DEV, so Plex will transcode on the CPU"
            );
            assert!(
                !grant.required,
                "{node} must not be required: most machines have no NVIDIA card"
            );
        }
    }

    #[test]
    fn media_is_granted_read_only_and_is_not_required() {
        // A server with no libraries configured is a normal first boot, not a fault.
        let library = PathBuf::from("/var/media/films");
        let grants = grants(&mount(), std::slice::from_ref(&library));
        let media = grants.iter().find(|g| g.path == library).unwrap();
        assert_eq!(media.access, access::READ_ONLY);
        assert!(!media.required);
    }

    #[test]
    fn the_render_node_is_granted_ioctl_because_va_api_needs_it() {
        // Read and write on /dev/dri is not enough: VA-API drives the GPU through
        // ioctls, and without IOCTL_DEV hardware transcoding fails looking exactly
        // like a missing driver -- which is the one diagnosis this project must not
        // produce falsely.
        let grants = grants(&mount(), &[]);
        let dri = grants
            .iter()
            .find(|g| g.path == Path::new("/dev/dri"))
            .unwrap();
        assert_ne!(dri.access & access::IOCTL_DEV, 0);
        assert!(
            !dri.required,
            "a machine with no GPU still runs Plex on the CPU"
        );
    }

    #[test]
    fn the_directories_plex_owns_are_the_two_it_writes_to() {
        let spec = spec(&mount(), "PlexOS", "0.1.0", "x86_64");
        assert_eq!(
            spec.owned_directories,
            vec![
                PathBuf::from(paths::PLEX_DATA),
                PathBuf::from(paths::PLEX_TRANSCODE_DIR)
            ]
        );
        assert!(
            !spec.owned_directories.contains(&mount()),
            "the app image is read-only and owned by nobody in particular"
        );
    }
}
