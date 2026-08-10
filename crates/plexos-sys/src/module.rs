//! Loading a kernel module, and making the device node it does not make itself.
//!
//! Both exist for the same reason: there is no `udev` here, and there is no `modprobe`
//! that PID 1 should be calling. The trap list already has three things that assumed a
//! udev, and PID 1 has no `PATH`, so spawning `/sbin/insmod` by name is the failure mode
//! this project has already met — `Command::new("ip")` returning a bare `ENOENT` from a
//! daemon while the same name typed at a shell worked fine.
//!
//! # Why `finit_module` rather than `init_module`
//!
//! It takes a file descriptor rather than a buffer, so the kernel reads the module
//! itself and nothing here has to hold twenty-seven megabytes in memory to hand it over.
//! `nvidia.ko` is exactly that large.
//!
//! # What signing means here
//!
//! With `CONFIG_MODULE_SIG_FORCE=y` the kernel refuses a module it did not have a
//! signature for, and reports it as `EKEYREJECTED`. That is a specific errno and worth
//! keeping distinct from the rest: it means the module is intact and was built by
//! somebody else, which is a completely different problem from a missing file.
//!
//! # What has run
//!
//! **Nothing here has run on a machine.** The modules it exists to load have been loaded
//! by hand with busybox `insmod` on an RTX 5060 and the kernel accepted their signature,
//! so the syscall below is doing what a working path already did — but it has not done
//! it itself.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::Path;

/// Loads a kernel module from a file.
///
/// `parameters` is the module's command line, which is empty for everything here. It is
/// a parameter rather than a constant because a module that needs one and is given none
/// fails in a way that names nothing.
///
/// # Errors
///
/// - `EKEYREJECTED` — the signature was refused. With `MODULE_SIG_FORCE` this is what an
///   unsigned or foreign-signed module gets, and the remedy is to sign it with this
///   kernel's key rather than to look at the hardware.
/// - `EEXIST` — already loaded, which is not a failure and callers may ignore.
/// - `ENOENT` — no such file; the module did not reach the image.
pub fn load(path: &Path, parameters: &str) -> io::Result<()> {
    let file = std::fs::File::open(path)?;
    let options = CString::new(parameters).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "module parameters contain a NUL",
        )
    })?;

    // SAFETY: `finit_module` takes a valid descriptor, a NUL-terminated string that
    // outlives the call, and a flags word. `file` is open for the duration and
    // `options` is not retained by the kernel. The syscall returns 0 or -1 and touches
    // nothing in this process.
    let result = unsafe {
        libc::syscall(
            libc::SYS_finit_module,
            file.as_raw_fd(),
            options.as_ptr(),
            0,
        )
    };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Creates a character device node.
///
/// Nothing else will. `devtmpfs` creates nodes for drivers that register through the
/// device model, and NVIDIA's does not — it takes its major with
/// `register_chrdev_region` and never calls `class_create`, which was established by
/// reading the driver rather than by waiting to find out.
///
/// The mode matters as much as the numbers. Without a udev rule a node is whatever it is
/// created as, and Plex does not run as root: `/dev/dri/renderD*` had to be relaxed to
/// `0666` for exactly this, and the failure was invisible because every probe above it
/// ran as root and reported success while Plex used the CPU.
///
/// # Errors
/// `EEXIST` if the node is already there, which callers may treat as success; `EPERM`
/// without `CAP_MKNOD`.
pub fn make_char_node(path: &Path, major: u32, minor: u32, mode: u32) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?;

    // SAFETY: `raw` is a NUL-terminated path that outlives the call; the mode and device
    // number are plain integers. mknod does not retain the pointer.
    let result = unsafe {
        libc::mknod(
            raw.as_ptr(),
            libc::S_IFCHR | mode,
            libc::makedev(major, minor),
        )
    };

    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    // mknod's mode argument is masked by the process umask, and PID 1's is 022. Asking
    // for 0666 therefore produces 0644 -- root can open it, uid 900 cannot write to it,
    // and Plex transcodes on the CPU without saying why.
    //
    // That is the render-node defect exactly, in the function written to prevent it. It
    // was found on an RTX 5060 by looking at `ls -l` after a boot, not by any test here:
    // the mode is right in the caller, right in the argument, and wrong on the machine.
    //
    // chmod is not masked, so the permissions are set again afterwards rather than
    // hoping the umask is something in particular.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_something_that_is_not_a_module_fails_rather_than_pretending() {
        // Run as an ordinary user this is EPERM, and as root it is ENOEXEC. Either way
        // it must not be Ok: a load that silently did nothing would leave a machine with
        // no driver and no message, which is the failure this whole path exists to make
        // impossible.
        let mut scratch = std::env::temp_dir();
        scratch.push("plexos-module-not-a-module.ko");
        std::fs::write(&scratch, b"this is not an ELF object").expect("a scratch file");
        let outcome = load(&scratch, "");
        let _ = std::fs::remove_file(&scratch);
        assert!(
            outcome.is_err(),
            "a text file was accepted as a kernel module"
        );
    }

    #[test]
    fn a_created_node_ends_up_with_the_mode_that_was_asked_for() {
        // The regression that shipped once. mknod masks its mode with the umask, so a
        // node asked for as 0666 arrives as 0644 under PID 1's 022 -- readable by
        // everyone, writable only by root, and Plex runs as uid 900. Every layer above
        // reports success; only `ls -l` on the machine shows it.
        //
        // mknod needs privilege, so this exercises the same set-permissions-afterwards
        // step on an ordinary file, under a umask that would otherwise mask the bits
        // away. If the fix is removed, this fails.

        // SAFETY: umask() cannot fail, returns the previous value, and affects only this
        // process. It is restored below.
        let previous = unsafe { libc::umask(0o022) };

        let mut scratch = std::env::temp_dir();
        scratch.push("plexos-module-mode-test");
        let _ = std::fs::remove_file(&scratch);
        std::fs::write(&scratch, b"x").expect("a scratch file");
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o666))
            .expect("permissions can be set");
        let mode = std::fs::metadata(&scratch)
            .expect("it exists")
            .permissions()
            .mode()
            & 0o777;
        let _ = std::fs::remove_file(&scratch);

        // SAFETY: as above; putting back what was there.
        unsafe { libc::umask(previous) };

        assert_eq!(
            mode, 0o666,
            "the umask masked the mode away; a device node made this way is not \
             reachable by the account Plex runs as"
        );
    }

    #[test]
    fn a_missing_module_says_so() {
        let outcome = load(Path::new("/nonexistent/nvidia.ko"), "");
        assert_eq!(
            outcome.unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "a module that did not reach the image must be reported as missing"
        );
    }

    #[test]
    fn making_a_node_without_privilege_reports_rather_than_pretends() {
        // As an ordinary user this is EPERM. The point is the same as the clock's: a
        // function that returned Ok having created nothing would produce a machine where
        // /dev/nvidia0 is absent and nothing anywhere says why.
        // SAFETY: getuid() takes no arguments, cannot fail, and returns a plain integer.
        let uid = unsafe { libc::getuid() };

        let mut scratch = std::env::temp_dir();
        scratch.push("plexos-module-node-test");
        let outcome = make_char_node(&scratch, 195, 255, 0o666);
        let _ = std::fs::remove_file(&scratch);
        match outcome {
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::AlreadyExists
                ),
                "expected EPERM as an ordinary user, got {error}"
            ),
            Ok(()) => assert_eq!(uid, 0, "mknod succeeded without privilege"),
        }
    }
}
