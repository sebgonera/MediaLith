//! Landlock: telling the kernel which paths a process may ever touch.
//!
//! ADR-0007 confines Plex to exactly its data directory, the configured media paths
//! read-only, the transcode directory, and the GPU render node. Landlock is how that is
//! said to the kernel, and its useful property is that it is **irreversible and
//! inherited**: once a thread restricts itself, nothing it does — and nothing it
//! `exec`s — can widen the set again.
//!
//! That is what makes it worth the syscalls. A media server parses untrusted files from
//! the internet all day, and the interesting question is not whether it has a bug but
//! what a bug can reach.
//!
//! # Deny by default, and only for what is handled
//!
//! Landlock denies what a ruleset *handles* and permits everything else. A ruleset that
//! handles nothing restricts nothing, and — this is the trap — succeeds silently while
//! doing so. [`Ruleset::new`] therefore takes the handled set explicitly and there is a
//! test that the set this project uses is not empty.
//!
//! # ABI versions
//!
//! The kernel grows access rights between versions, and asking for one it does not know
//! fails the whole call with `EINVAL` rather than ignoring the bit. The version is
//! queried first and the request masked down to it, so a newer PlexOS on an older
//! kernel loses precision rather than losing confinement altogether.
//!
//! # What is verified and what is not
//!
//! The constants are pinned against `include/uapi/linux/landlock.h` and the syscall
//! numbers against `arch/x86/entry/syscalls/syscall_64.tbl` in the kernel this image
//! builds. **Nothing here has run on the appliance.** Delete this notice when it has.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::Path;

/// `landlock_create_ruleset`, from `syscall_64.tbl`.
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
/// `landlock_add_rule`.
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
/// `landlock_restrict_self`.
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

/// Ask for the supported ABI version rather than create a ruleset.
const CREATE_RULESET_VERSION: u32 = 1 << 0;

/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: libc::c_int = 1;

/// Filesystem access rights, from `include/uapi/linux/landlock.h`.
///
/// Named rather than spelled as literals at each call site: `1 << 14` says nothing
/// about truncation, and a wrong bit here silently grants or denies the wrong thing.
pub mod access {
    /// Execute a file.
    pub const EXECUTE: u64 = 1 << 0;
    /// Open a file for writing.
    pub const WRITE_FILE: u64 = 1 << 1;
    /// Open a file for reading.
    pub const READ_FILE: u64 = 1 << 2;
    /// List a directory.
    pub const READ_DIR: u64 = 1 << 3;
    /// Remove a directory.
    pub const REMOVE_DIR: u64 = 1 << 4;
    /// Unlink a file.
    pub const REMOVE_FILE: u64 = 1 << 5;
    /// Create a character device.
    pub const MAKE_CHAR: u64 = 1 << 6;
    /// Create a directory.
    pub const MAKE_DIR: u64 = 1 << 7;
    /// Create a regular file.
    pub const MAKE_REG: u64 = 1 << 8;
    /// Create a UNIX socket.
    pub const MAKE_SOCK: u64 = 1 << 9;
    /// Create a FIFO.
    pub const MAKE_FIFO: u64 = 1 << 10;
    /// Create a block device.
    pub const MAKE_BLOCK: u64 = 1 << 11;
    /// Create a symbolic link.
    pub const MAKE_SYM: u64 = 1 << 12;
    /// Link or rename across directories.
    pub const REFER: u64 = 1 << 13;
    /// Truncate a file.
    pub const TRUNCATE: u64 = 1 << 14;
    /// `ioctl` on a device file.
    pub const IOCTL_DEV: u64 = 1 << 15;

    /// Everything defined at ABI 5, which is what this kernel offers.
    ///
    /// Used as the *handled* set: a ruleset restricts only what it handles, so handling
    /// everything and then granting back what is wanted is what makes the policy a
    /// deny-list of one line rather than an allow-list with silent gaps.
    pub const ALL: u64 = EXECUTE
        | WRITE_FILE
        | READ_FILE
        | READ_DIR
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM
        | REFER
        | TRUNCATE
        | IOCTL_DEV;

    /// Reading a directory tree and executing from it. What a mounted app image needs.
    pub const READ_EXECUTE: u64 = EXECUTE | READ_FILE | READ_DIR;

    /// Reading a directory tree. What a media library needs.
    pub const READ_ONLY: u64 = READ_FILE | READ_DIR;

    /// Everything a process needs to own a directory: read, write, create, delete.
    pub const READ_WRITE: u64 = READ_FILE
        | READ_DIR
        | WRITE_FILE
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_SYM
        | REFER
        | TRUNCATE;
}

/// The ABI as the running kernel reports it.
///
/// Which rights exist depends on this, and asking for one the kernel does not know
/// fails the entire call with `EINVAL` rather than ignoring the bit.
///
/// # Errors
/// Fails when Landlock is not compiled in or not enabled in `CONFIG_LSM`, which is
/// worth telling apart from a policy that failed to apply.
pub fn abi_version() -> io::Result<i32> {
    // SAFETY: a plain syscall with a null pointer and a zero length, which is exactly
    // what LANDLOCK_CREATE_RULESET_VERSION documents as its calling convention. No
    // memory is read or written by the kernel.
    let result = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<libc::c_void>(),
            0_usize,
            CREATE_RULESET_VERSION,
        )
    };
    if result < 0 {
        return Err(annotate(
            &io::Error::last_os_error(),
            "Landlock is unavailable. Check CONFIG_SECURITY_LANDLOCK is set and that \
             `landlock` appears in CONFIG_LSM — it is inert if the kernel was built \
             with it but the LSM list omits it.",
        ));
    }
    i32::try_from(result).map_err(|_| {
        io::Error::other(format!(
            "the kernel reported Landlock ABI {result}, which is not a version"
        ))
    })
}

/// Rights that exist at a given ABI version.
///
/// Masking rather than failing means a newer PlexOS on an older kernel loses precision
/// instead of losing confinement, which is the safer way round: a ruleset that fails to
/// apply leaves the process unconfined.
#[must_use]
pub fn rights_at_abi(abi: i32) -> u64 {
    let mut rights = access::ALL;
    if abi < 5 {
        rights &= !access::IOCTL_DEV;
    }
    if abi < 3 {
        rights &= !access::TRUNCATE;
    }
    if abi < 2 {
        rights &= !access::REFER;
    }
    rights
}

/// A policy under construction.
///
/// Nothing takes effect until [`Ruleset::enforce`], and after it nothing can be undone.
#[derive(Debug)]
pub struct Ruleset {
    fd: OwnedFd,
    handled: u64,
}

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

// packed, per the UAPI header: the kernel checks the size and a compiler-inserted
// four-byte tail would make this a different structure than the one it expects.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

impl Ruleset {
    /// Starts a ruleset handling `handled_access_fs`.
    ///
    /// Landlock denies only what a ruleset handles, so this is the set being taken
    /// away; [`Ruleset::allow`] gives parts of it back for named paths. Handling
    /// nothing produces a ruleset that applies cleanly and restricts nothing at all.
    ///
    /// # Errors
    /// Fails if Landlock is unavailable, or if `handled_access_fs` is empty — which the
    /// kernel accepts and this refuses, because a confinement that confines nothing is
    /// worse than none: it looks applied.
    pub fn new(handled_access_fs: u64) -> io::Result<Self> {
        if handled_access_fs == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a Landlock ruleset that handles no access rights restricts nothing and \
                 succeeds while doing so. Refusing rather than applying it.",
            ));
        }

        let abi = abi_version()?;
        let handled = handled_access_fs & rights_at_abi(abi);
        if handled == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "none of the requested access rights exist at Landlock ABI {abi}, so \
                     nothing would be restricted"
                ),
            ));
        }

        let attr = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: 0,
            scoped: 0,
        };

        // SAFETY: the pointer is to a live, correctly-shaped RulesetAttr that outlives
        // the call, and the length given is its true size. The kernel reads it and
        // retains nothing. A negative return is an error, checked below.
        let raw = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::from_ref::<RulesetAttr>(&attr),
                std::mem::size_of::<RulesetAttr>(),
                0_u32,
            )
        };
        if raw < 0 {
            return Err(annotate(
                &io::Error::last_os_error(),
                "could not create a Landlock ruleset",
            ));
        }

        let fd = i32::try_from(raw)
            .map_err(|_| io::Error::other("landlock_create_ruleset returned a bad fd"))?;
        // SAFETY: the kernel just returned this descriptor and nothing else owns it, so
        // taking ownership here is the only claim on it.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };

        Ok(Self { fd, handled })
    }

    /// Grants `allowed` on everything beneath `path`.
    ///
    /// A path that does not exist is an error rather than a silent omission: a policy
    /// that quietly drops the rule for a media directory produces a Plex that cannot
    /// read the library, and the reason would appear nowhere.
    ///
    /// # Errors
    /// If the path cannot be opened, or the rule cannot be added.
    pub fn allow(&mut self, path: &Path, allowed: u64) -> io::Result<()> {
        let c_path = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} contains a NUL", path.display()),
            )
        })?;

        // O_PATH: the descriptor names the file without opening it for I/O, which is
        // all Landlock needs and is what lets a directory be referenced without the
        // permission to read it.
        // SAFETY: the pointer is to a NUL-terminated string that outlives the call.
        let raw = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(annotate(
                &io::Error::last_os_error(),
                &format!(
                    "could not open {} to grant access to it. The rule cannot be added, \
                     and applying the policy without it would confine Plex out of a \
                     directory it needs.",
                    path.display()
                ),
            ));
        }
        // SAFETY: open(2) just returned this and nothing else owns it. Wrapping it now
        // means it is closed however this function exits.
        let handle = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };

        let attr = PathBeneathAttr {
            allowed_access: allowed & self.handled,
            parent_fd: handle.as_raw_fd(),
        };

        // SAFETY: the pointer is to a live, packed PathBeneathAttr matching the UAPI
        // layout, valid for the duration of the call; `handle` is open throughout.
        let result = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                self.fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                std::ptr::from_ref::<PathBeneathAttr>(&attr),
                0_u32,
            )
        };
        if result < 0 {
            return Err(annotate(
                &io::Error::last_os_error(),
                &format!("could not grant access beneath {}", path.display()),
            ));
        }
        Ok(())
    }

    /// Applies the policy to this thread and everything it goes on to `exec`.
    ///
    /// Irreversible. Sets `PR_SET_NO_NEW_PRIVS` first, which
    /// `landlock_restrict_self` requires of anything without `CAP_SYS_ADMIN` — and
    /// which is wanted in its own right, since it is what stops a setuid binary
    /// reached from inside the sandbox from escaping it.
    ///
    /// # Errors
    /// If either step fails. Both are fatal to the caller's intent: a process that
    /// meant to confine itself and did not must not carry on as though it had.
    pub fn enforce(self) -> io::Result<()> {
        // SAFETY: prctl with PR_SET_NO_NEW_PRIVS takes scalar arguments only; no
        // pointer is dereferenced.
        let no_new_privs = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if no_new_privs != 0 {
            return Err(annotate(
                &io::Error::last_os_error(),
                "could not set no_new_privs, which Landlock requires",
            ));
        }

        // SAFETY: a scalar syscall taking the ruleset descriptor, which is open and
        // owned by `self` for the duration of the call.
        let result =
            unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, self.fd.as_raw_fd(), 0_u32) };
        if result < 0 {
            return Err(annotate(
                &io::Error::last_os_error(),
                "could not apply the Landlock ruleset",
            ));
        }
        Ok(())
    }

    /// The rights this ruleset takes away, after masking to the kernel's ABI.
    #[must_use]
    pub const fn handled(&self) -> u64 {
        self.handled
    }
}

/// Adds context to an errno without losing its kind.
fn annotate(error: &io::Error, context: &str) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_syscall_numbers_are_the_kernels() {
        // Pinned against arch/x86/entry/syscalls/syscall_64.tbl. These are not ours to
        // choose, and a wrong number calls an unrelated syscall rather than failing.
        assert_eq!(SYS_LANDLOCK_CREATE_RULESET, 444);
        assert_eq!(SYS_LANDLOCK_ADD_RULE, 445);
        assert_eq!(SYS_LANDLOCK_RESTRICT_SELF, 446);
    }

    #[test]
    fn the_access_bits_are_the_uapi_values() {
        // include/uapi/linux/landlock.h. A wrong bit grants or denies something other
        // than what the call site asked for, silently and in the direction nobody
        // checks.
        assert_eq!(access::EXECUTE, 1 << 0);
        assert_eq!(access::WRITE_FILE, 1 << 1);
        assert_eq!(access::READ_FILE, 1 << 2);
        assert_eq!(access::READ_DIR, 1 << 3);
        assert_eq!(access::REFER, 1 << 13);
        assert_eq!(access::TRUNCATE, 1 << 14);
        assert_eq!(access::IOCTL_DEV, 1 << 15);
    }

    #[test]
    fn the_path_beneath_attribute_is_packed_the_way_the_kernel_reads_it() {
        // 8 bytes of access plus 4 of fd. Unpacked, the compiler pads to 16 and the
        // kernel rejects the size -- or worse, an older kernel reads 12 bytes of a
        // 16-byte struct and takes padding for a descriptor.
        assert_eq!(std::mem::size_of::<PathBeneathAttr>(), 12);
    }

    #[test]
    fn the_ruleset_attribute_matches_the_three_fields_of_the_uapi_struct() {
        assert_eq!(std::mem::size_of::<RulesetAttr>(), 24);
    }

    #[test]
    fn handling_nothing_is_refused_rather_than_applied() {
        // The quiet failure this whole module has to avoid: Landlock restricts only
        // what a ruleset handles, so an empty set applies cleanly and confines nothing.
        // A caller would see success and believe Plex was sandboxed.
        let error = Ruleset::new(0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("restricts nothing"), "{error}");
    }

    #[test]
    fn older_abis_lose_rights_rather_than_the_whole_policy() {
        // Asking for a right the kernel does not know fails the entire call with
        // EINVAL, which would leave the process unconfined. Masking down loses
        // precision instead, which is the safer direction.
        assert_eq!(rights_at_abi(5) & access::IOCTL_DEV, access::IOCTL_DEV);
        assert_eq!(rights_at_abi(4) & access::IOCTL_DEV, 0);
        assert_eq!(rights_at_abi(2) & access::TRUNCATE, 0);
        assert_eq!(rights_at_abi(1) & access::REFER, 0);
        assert_ne!(
            rights_at_abi(1) & access::READ_FILE,
            0,
            "the basics survive"
        );
    }

    #[test]
    fn the_composite_sets_say_what_their_names_say() {
        // Table-driven so the assertions are over values rather than over constants
        // the compiler folds away -- a folded assertion is one clippy is right to say
        // proves nothing at run time.
        let cases: [(&str, u64, u64, bool); 7] = [
            (
                "read-only writes nothing",
                access::READ_ONLY,
                access::WRITE_FILE,
                false,
            ),
            (
                "read-only executes nothing",
                access::READ_ONLY,
                access::EXECUTE,
                false,
            ),
            (
                "read-execute writes nothing",
                access::READ_EXECUTE,
                access::WRITE_FILE,
                false,
            ),
            (
                "read-execute executes",
                access::READ_EXECUTE,
                access::EXECUTE,
                true,
            ),
            (
                "read-write writes",
                access::READ_WRITE,
                access::WRITE_FILE,
                true,
            ),
            // A media server has no business creating device nodes where it can write.
            (
                "no character devices",
                access::READ_WRITE,
                access::MAKE_CHAR,
                false,
            ),
            (
                "no block devices",
                access::READ_WRITE,
                access::MAKE_BLOCK,
                false,
            ),
        ];
        for (name, set, bit, expected) in cases {
            assert_eq!((set & bit) != 0, expected, "{name}");
        }
    }

    #[test]
    fn everything_is_handled_so_the_policy_is_an_allow_list() {
        // ALL must cover every bit the composites grant, or a right could be given back
        // that was never taken away -- which reads as a grant and is in fact a hole.
        for granted in [access::READ_ONLY, access::READ_EXECUTE, access::READ_WRITE] {
            assert_eq!(granted & !access::ALL, 0, "{granted:#x} is not all handled");
        }
    }
}
