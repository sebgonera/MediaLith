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
//! The spec and the ordering are tested. **Plex has never been started by this.**
//! Delete this notice when it has.

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
        // The app image. Read and execute, never write: it is a read-only erofs mount
        // and saying so here means a future writable one does not silently become
        // writable to Plex.
        Grant {
            path: mount.to_path_buf(),
            access: access::READ_EXECUTE,
            required: true,
        },
        // Its own data. The media database lives here.
        Grant {
            path: PathBuf::from(paths::PLEX_DATA),
            access: access::READ_WRITE,
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
    ];

    // Media, read-only. Not required: a server with no libraries configured yet is a
    // normal first-boot state, not a broken one.
    grants.extend(media.iter().map(|path| Grant {
        path: path.clone(),
        access: access::READ_ONLY,
        required: false,
    }));

    grants
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
    fn nothing_writable_is_also_executable() {
        // The property worth having: a directory Plex can write to is a directory an
        // exploit can drop a binary into, and one it can also execute from is a
        // complete escape. Media is read-only, scratch and data are not executable.
        for grant in grants(&mount(), &[PathBuf::from("/var/media/films")]) {
            let writable = grant.access & access::WRITE_FILE != 0;
            let executable = grant.access & access::EXECUTE != 0;
            assert!(
                !(writable && executable),
                "{} is both writable and executable",
                grant.path.display()
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
