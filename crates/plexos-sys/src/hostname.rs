//! The machine's name, as the kernel holds it.
//!
//! An appliance with no keyboard is found by name or not at all, and until now MediaLith had
//! none: nothing ever called `sethostname`, so the kernel's default stood and every log
//! line, every DHCP request and every future mDNS announcement carried it.
//!
//! # Two places, and both are needed
//!
//! The kernel's hostname is process-visible immediately and forgotten at reboot.
//! `/etc/hostname` is what re-establishes it on the next boot and what other programs
//! read when they want the name without a syscall. Setting one without the other gives a
//! machine that renames itself back overnight, or one that reports two different names
//! depending on who is asked — and the second is worse, because it looks like it worked.
//!
//! Writing the file is not this crate's business: `/etc` is an overlay assembled by
//! `plexos-init`, and a syscall crate that started writing configuration would be doing
//! two jobs. [`set`] does the syscall; the caller owns the file.
//!
//! # What has run
//!
//! **Nothing.** No hostname has ever been set on the appliance.

use std::io;

/// The longest name the kernel will accept.
///
/// `__NEW_UTS_LEN` in `include/uapi/linux/utsname.h` is 64, and `sethostname` refuses
/// anything longer with `EINVAL` — an error about arguments, which reads as a malformed
/// call rather than as a name three characters too long. Checked here so the diagnostic
/// can say which it is.
pub const MAX_LEN: usize = 64;

/// Sets the kernel's hostname.
///
/// # Errors
/// `EPERM` without `CAP_SYS_ADMIN`, or `EINVAL` for a name this rejects first.
pub fn set(name: &str) -> io::Result<()> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a hostname cannot be empty. Remedy: the kernel accepts an empty name and \
             then nothing can be found by it, so this refuses instead.",
        ));
    }

    if name.len() > MAX_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a hostname may be at most {MAX_LEN} bytes and this is {}. Remedy: \
                 shorten it. The kernel reports this as EINVAL, which reads as a broken \
                 call rather than as a name that is slightly too long.",
                name.len()
            ),
        ));
    }

    // Rejected here rather than passed through, because a NUL would truncate the name at
    // the syscall boundary and the machine would silently take a shorter one.
    if name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a hostname cannot contain a NUL byte. Remedy: it would be silently \
             truncated at the syscall boundary rather than refused.",
        ));
    }

    // SAFETY: sethostname() reads `len` bytes from the pointer and copies them into the
    // kernel's UTS namespace. The pointer and length come from one live `&str`, which
    // guarantees the region is valid and initialised for the whole call; the length is
    // checked above against the kernel's own limit, so the kernel never reads past the
    // slice. Nothing is retained after the call returns, and no memory is handed over.
    let result = unsafe { libc::sethostname(name.as_ptr().cast::<libc::c_char>(), name.len()) };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Reads the kernel's current hostname.
///
/// # Errors
/// If the kernel refuses the call, which in practice it does not.
pub fn get() -> io::Result<String> {
    // One byte over the limit so that a name of exactly MAX_LEN still has room for the
    // terminator the kernel writes when there is space for one.
    let mut buffer = vec![0u8; MAX_LEN + 1];

    // SAFETY: gethostname() writes at most `len` bytes into the pointer. The pointer and
    // length describe one live, fully initialised Vec allocation, so the kernel cannot
    // write outside it. The buffer outlives the call and is not aliased.
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };

    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    // The kernel may or may not terminate the name when it exactly fills the buffer, so
    // the end is found rather than assumed.
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    buffer.truncate(end);

    String::from_utf8(buffer).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Whether a name is one this appliance should accept.
///
/// Deliberately narrower than the kernel, which accepts almost anything. A hostname is
/// about to be handed to a DHCP server, written into logs, and one day announced over
/// mDNS, and each of those has its own opinion about what is legal. RFC 1123's letters,
/// digits and hyphens is the intersection that nothing objects to.
///
/// Rejecting is cheap and the alternative is a name that works everywhere except the one
/// place somebody needs it.
#[must_use]
pub fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_LEN
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_the_kernels_own() {
        // __NEW_UTS_LEN in include/uapi/linux/utsname.h. Pinned against the value from
        // that header rather than against anything computed here: where a constant comes
        // from somewhere else, the code is what changes when this fails.
        assert_eq!(MAX_LEN, 64);
    }

    #[test]
    fn a_name_that_is_merely_too_long_says_so_rather_than_reporting_einval() {
        let long = "a".repeat(MAX_LEN + 1);
        let error = set(&long).expect_err("must refuse");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("at most 64 bytes"), "{error}");
        assert!(error.to_string().contains("Remedy:"), "{error}");
    }

    #[test]
    fn an_empty_name_is_refused_although_the_kernel_would_take_it() {
        let error = set("").expect_err("must refuse");
        assert!(error.to_string().contains("Remedy:"), "{error}");
    }

    #[test]
    fn a_nul_is_refused_rather_than_silently_truncating() {
        // The failure this prevents is the quiet one: the kernel would take everything
        // before the NUL and the machine would carry a name nobody chose.
        assert!(set("plex\0os").is_err());
    }

    #[test]
    fn validity_is_the_intersection_that_nothing_objects_to() {
        assert!(is_valid("plexos"));
        assert!(is_valid("plex-os-1"));
        assert!(is_valid(&"a".repeat(MAX_LEN)));

        assert!(!is_valid(""), "nothing can be found by an empty name");
        assert!(!is_valid("-leading"), "RFC 1123 forbids a leading hyphen");
        assert!(!is_valid("trailing-"), "and a trailing one");
        assert!(!is_valid("has space"));
        assert!(
            !is_valid("under_score"),
            "legal in DNS labels nowhere useful"
        );
        assert!(
            !is_valid("dots.are.not.a.hostname"),
            "this is a name, not an FQDN"
        );
        assert!(!is_valid(&"a".repeat(MAX_LEN + 1)));
    }

    #[test]
    fn the_current_hostname_can_be_read() {
        // Reading needs no privilege, so this runs anywhere. Setting needs CAP_SYS_ADMIN
        // and is therefore not tested here -- it would either be skipped on a normal
        // build host or change the hostname of whatever machine ran the suite.
        let name = get().expect("gethostname does not fail in practice");
        assert!(name.len() <= MAX_LEN, "got {name:?}");
    }
}
