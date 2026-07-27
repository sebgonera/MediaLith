//! Mounting, moving mounts, and replacing the root.
//!
//! `mount(2)` takes its options two ways at once: some are bits in a flags word, and
//! the rest are a filesystem-specific string. `nosuid` is a bit; `mode=0755` is not.
//! Passing a bit-option through as string data does not fail — the filesystem ignores
//! what it does not recognise — so a mount that should have been `nosuid` is silently
//! not, and `/var` ends up honouring setuid bits on a partition that holds
//! user-supplied media. [`parse_options`] is therefore a pure function with tests
//! rather than something done inline.
//!
//! # `switch_root`
//!
//! There is no `switch_root` syscall. The name refers to a specific sequence, and
//! every step of it matters:
//!
//! 1. `chdir(new_root)`
//! 2. `mount(".", "/", MS_MOVE)` — the new root becomes `/`
//! 3. `chroot(".")`
//! 4. `chdir("/")` — without this the working directory still refers to the old root
//! 5. `execve(init)`
//!
//! Doing this rather than `pivot_root` is what the initrd case calls for: the old
//! root is a rootfs that cannot be unmounted, and `MS_MOVE` over it discards it
//! wholesale along with everything still open on it.

use std::ffi::CString;
use std::io;
use std::path::Path;

/// Mount flags, named as `mount(2)` names them.
pub mod flags {
    /// Read-only.
    pub const RDONLY: u64 = 1;
    /// Ignore setuid and setgid bits.
    pub const NOSUID: u64 = 2;
    /// Disallow access to device special files.
    pub const NODEV: u64 = 4;
    /// Disallow program execution.
    pub const NOEXEC: u64 = 8;
    /// Atomically move a subtree.
    pub const MOVE: u64 = 8192;
    /// Bind mount.
    pub const BIND: u64 = 4096;
    /// Do not update access times.
    pub const NOATIME: u64 = 1024;
    /// Update access times relative to modify time.
    pub const RELATIME: u64 = 1 << 21;
}

/// Option names that are flag bits rather than filesystem data.
///
/// `rw` and `defaults` map to zero: they are the absence of other bits, and passing
/// them through as data would have some filesystems reject the whole option string.
const FLAG_OPTIONS: &[(&str, u64)] = &[
    ("ro", flags::RDONLY),
    ("rw", 0),
    ("defaults", 0),
    ("nosuid", flags::NOSUID),
    ("nodev", flags::NODEV),
    ("noexec", flags::NOEXEC),
    ("noatime", flags::NOATIME),
    ("relatime", flags::RELATIME),
    ("bind", flags::BIND),
];

/// Splits a comma-separated option string into a flags word and filesystem data.
///
/// Empty fields are dropped, so a trailing comma cannot produce an empty data option
/// that some filesystems reject.
#[must_use]
pub fn parse_options(options: &str) -> (u64, String) {
    let mut bits = 0u64;
    let mut data: Vec<&str> = Vec::new();

    for option in options.split(',').filter(|o| !o.is_empty()) {
        match FLAG_OPTIONS.iter().find(|(name, _)| *name == option) {
            Some((_, bit)) => bits |= bit,
            None => data.push(option),
        }
    }
    (bits, data.join(","))
}

fn c_string(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{value:?} contains a NUL"),
        )
    })
}

/// Mounts a filesystem.
///
/// # Errors
///
/// Any failure of `mount(2)`, with the path included so the message says which mount
/// failed rather than only why.
pub fn mount(source: &str, target: &str, fstype: &str, options: &str) -> io::Result<()> {
    let (bits, data) = parse_options(options);
    let c_source = c_string(source)?;
    let c_target = c_string(target)?;
    let c_fstype = c_string(fstype)?;
    let c_data = c_string(&data)?;

    // SAFETY: all four pointers are to NUL-terminated strings that live until the end
    // of this function, which outlives the call. mount(2) copies what it needs and
    // retains none of them. The flags word is a plain integer.
    let result = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            bits,
            c_data.as_ptr().cast::<libc::c_void>(),
        )
    };

    if result < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!(
                "mounting {source} at {target} as {fstype} ({options}): {}",
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

/// Moves an existing mount to a new location, with everything open on it intact.
///
/// # Errors
///
/// Any failure of `mount(2)` with `MS_MOVE`.
pub fn move_mount(from: &str, to: &str) -> io::Result<()> {
    let c_from = c_string(from)?;
    let c_to = c_string(to)?;

    // SAFETY: both pointers are to NUL-terminated strings outliving the call. MS_MOVE
    // ignores the fstype and data arguments, so null is the documented value for them.
    let result = unsafe {
        libc::mount(
            c_from.as_ptr(),
            c_to.as_ptr(),
            std::ptr::null(),
            flags::MOVE,
            std::ptr::null(),
        )
    };

    if result < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!(
                "moving mount {from} to {to}: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

/// Replaces the root filesystem with `new_root` and executes `init`.
///
/// Does not return on success: the process image is replaced.
///
/// # Errors
///
/// Any failure of the sequence. Each error names the step, because "No such file or
/// directory" on its own could mean the new root, the init binary, or neither.
pub fn switch_root(
    new_root: &str,
    init: &str,
    args: &[&str],
) -> io::Result<std::convert::Infallible> {
    let c_new_root = c_string(new_root)?;
    let c_dot = c_string(".")?;
    let c_slash = c_string("/")?;

    if !Path::new(new_root)
        .join(init.trim_start_matches('/'))
        .exists()
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{init} does not exist inside {new_root}; the /usr image is mounted \
                 but does not contain an init, so this slot cannot be booted"
            ),
        ));
    }

    // SAFETY: c_new_root is a valid NUL-terminated string outliving the call.
    let result = unsafe { libc::chdir(c_new_root.as_ptr()) };
    if result < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("chdir to {new_root}: {}", io::Error::last_os_error()),
        ));
    }

    // SAFETY: both pointers are valid NUL-terminated strings outliving the call.
    // MS_MOVE ignores fstype and data, for which null is documented.
    let result = unsafe {
        libc::mount(
            c_dot.as_ptr(),
            c_slash.as_ptr(),
            std::ptr::null(),
            flags::MOVE,
            std::ptr::null(),
        )
    };
    if result < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("moving {new_root} onto /: {}", io::Error::last_os_error()),
        ));
    }

    // SAFETY: c_dot is a valid NUL-terminated string outliving the call.
    let result = unsafe { libc::chroot(c_dot.as_ptr()) };
    if result < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("chroot: {}", io::Error::last_os_error()),
        ));
    }

    // Without this the working directory still refers to the old root, and every
    // relative path afterwards resolves outside the new one.
    // SAFETY: c_slash is a valid NUL-terminated string outliving the call.
    let result = unsafe { libc::chdir(c_slash.as_ptr()) };
    if result < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("chdir to /: {}", io::Error::last_os_error()),
        ));
    }

    // The new init is told which role it is playing. Without an argument it would
    // read the same command line, compute the same plan, and run it again -- which
    // is exactly what the first booting image did, failing at verity with EBUSY
    // because the device it was about to create already existed.
    crate::process::exec(init, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_values_match_the_system_headers() {
        // Printed by a C program compiled against <sys/mount.h>, not derived here and
        // compared to itself. A wrong bit does not fail the mount: it applies some
        // other protection, or none, and the mount succeeds looking correct. MS_MOVE
        // being wrong is worse still, since switch_root would move nothing and then
        // chroot into the old root.
        assert_eq!(flags::RDONLY, 1);
        assert_eq!(flags::NOSUID, 2);
        assert_eq!(flags::NODEV, 4);
        assert_eq!(flags::NOEXEC, 8);
        assert_eq!(flags::NOATIME, 1024);
        assert_eq!(flags::BIND, 4096);
        assert_eq!(flags::MOVE, 8192);
        assert_eq!(flags::RELATIME, 2_097_152);
    }

    #[test]
    fn flag_options_become_bits_not_data() {
        let (bits, data) = parse_options("nosuid,nodev,noexec");
        assert_eq!(bits, flags::NOSUID | flags::NODEV | flags::NOEXEC);
        assert!(data.is_empty(), "{data:?}");
    }

    #[test]
    fn filesystem_options_stay_as_data() {
        let (bits, data) = parse_options("mode=0755,size=64m");
        assert_eq!(bits, 0);
        assert_eq!(data, "mode=0755,size=64m");
    }

    #[test]
    fn a_mixed_option_string_is_split_correctly() {
        // This is exactly what the boot plan produces for /run and /dev.
        let (bits, data) = parse_options("nosuid,nodev,mode=0755");
        assert_eq!(bits, flags::NOSUID | flags::NODEV);
        assert_eq!(data, "mode=0755");
    }

    #[test]
    fn the_security_options_the_boot_plan_relies_on_are_all_recognised() {
        // If any of these silently fell through to data, the mount would succeed
        // without the protection. /var holds executable app images (ADR-0007), so
        // nosuid and nodev there are a real boundary, not hygiene.
        for (option, bit) in [
            ("nosuid", flags::NOSUID),
            ("nodev", flags::NODEV),
            ("noexec", flags::NOEXEC),
            ("ro", flags::RDONLY),
        ] {
            let (bits, data) = parse_options(option);
            assert_eq!(bits, bit, "{option} did not become a flag");
            assert!(data.is_empty(), "{option} leaked into data as {data:?}");
        }
    }

    #[test]
    fn ro_and_rw_do_not_both_set_a_bit() {
        assert_eq!(parse_options("ro").0, flags::RDONLY);
        assert_eq!(parse_options("rw").0, 0);
        // "rw" must not appear in data either; some filesystems reject it there.
        assert!(parse_options("rw,nosuid").1.is_empty());
    }

    #[test]
    fn empty_fields_are_dropped() {
        // A trailing comma would otherwise produce an empty data option.
        let (bits, data) = parse_options("nosuid,,");
        assert_eq!(bits, flags::NOSUID);
        assert!(data.is_empty(), "{data:?}");
        assert_eq!(parse_options("").1, "");
    }

    #[test]
    fn the_usr_mount_from_the_boot_plan_is_read_only_and_locked_down() {
        let (bits, data) = parse_options("ro,nodev,nosuid");
        assert_eq!(bits & flags::RDONLY, flags::RDONLY);
        assert_eq!(bits & flags::NOSUID, flags::NOSUID);
        assert_eq!(bits & flags::NODEV, flags::NODEV);
        assert!(data.is_empty());
    }

    #[test]
    fn the_overlay_options_survive_as_data() {
        // lowerdir/upperdir/workdir are overlayfs data and must reach the filesystem
        // untouched; losing them mounts an empty overlay over /etc.
        let options = "lowerdir=/sysroot/usr/share/factory/etc,\
                       upperdir=/sysroot/var/lib/plexos/etc,\
                       workdir=/sysroot/var/lib/plexos/.etc-work";
        let (bits, data) = parse_options(options);
        assert_eq!(bits, 0);
        assert!(data.contains("lowerdir="));
        assert!(data.contains("upperdir="));
        assert!(data.contains("workdir="));
    }

    #[test]
    fn a_nul_in_a_path_is_refused_rather_than_truncated() {
        let error = mount("src\0evil", "/target", "tmpfs", "").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn mount_errors_name_the_paths_involved() {
        // "Operation not permitted" alone does not say which of a dozen mounts failed.
        let error = mount("/nonexistent-source", "/nonexistent-target", "ext4", "ro").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("/nonexistent-source"), "{text}");
        assert!(text.contains("/nonexistent-target"), "{text}");
        assert!(text.contains("ext4"), "{text}");
    }

    #[test]
    fn switch_root_refuses_when_the_init_is_not_in_the_new_root() {
        // Checked before anything is moved, because after MS_MOVE there is no way
        // back and the kernel panics on a PID 1 that could not be executed.
        let error = switch_root("/nonexistent-root", "/usr/bin/plexos-init", &[]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("cannot be booted"), "{error}");
    }
}
