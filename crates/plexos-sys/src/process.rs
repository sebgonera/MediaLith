//! Replacing the current process image.
//!
//! Separate from [`crate::mount`] because `switch_root` is a mount operation that
//! happens to end in an exec, whereas this is an exec on its own. `plexos-init` needs
//! both: once to hand the initrd over to the verified `/usr`, and once more when the
//! supervisor starts something in its own right.

use std::ffi::CString;
use std::io;

fn c_string(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{value:?} contains a NUL"),
        )
    })
}

/// Replaces this process with `program`.
///
/// `argv[0]` is set to `program`, and `args` follow it.
///
/// # Errors
///
/// Only on failure: on success there is no process left to return to. A failure here
/// as PID 1 means the kernel panics immediately afterwards, so the error text is the
/// last thing anyone will see.
pub fn exec(program: &str, args: &[&str]) -> io::Result<std::convert::Infallible> {
    let c_program = c_string(program)?;
    let c_args: Vec<CString> = args
        .iter()
        .map(|arg| c_string(arg))
        .collect::<io::Result<_>>()?;

    // argv is argv[0] followed by the arguments, then a NULL terminator.
    let mut pointers: Vec<*const libc::c_char> = Vec::with_capacity(c_args.len() + 2);
    pointers.push(c_program.as_ptr());
    pointers.extend(c_args.iter().map(|arg| arg.as_ptr()));
    pointers.push(std::ptr::null());

    // SAFETY: c_program and every element of c_args live until the end of this
    // function, so the pointers are valid for the duration of the call, and the list
    // is NULL-terminated as execv requires. On success execv does not return, so
    // nothing can observe the borrowed pointers afterwards.
    unsafe { libc::execv(c_program.as_ptr(), pointers.as_ptr()) };

    Err(io::Error::new(
        io::Error::last_os_error().kind(),
        format!("executing {program}: {}", io::Error::last_os_error()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_reports_its_name() {
        // "No such file or directory" alone, printed as the last line before a
        // kernel panic, does not say which program was missing.
        let error = exec("/nonexistent-program", &[]).unwrap_err();
        assert!(
            error.to_string().contains("/nonexistent-program"),
            "{error}"
        );
    }

    #[test]
    fn a_nul_in_an_argument_is_refused() {
        let error = exec("/bin/sh", &["-c\0evil"]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
