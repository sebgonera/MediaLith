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

/// Replaces this process with `program`, giving it exactly `environment`.
///
/// `execve` rather than `execv`, and the difference is the point: the environment is
/// *replaced*, not added to. A confined process should not inherit whatever happened to
/// be in the parent's environment, and on this system the parent's environment is
/// whatever PID 1 was handed — which is nothing, so inheriting it silently gives the
/// child no `PATH` at all.
///
/// # Errors
///
/// Only on failure; on success there is no process left to return to.
pub fn exec_with_env(
    program: &str,
    args: &[&str],
    environment: &[(&str, &str)],
) -> io::Result<std::convert::Infallible> {
    let c_program = c_string(program)?;
    let c_args: Vec<CString> = args
        .iter()
        .map(|arg| c_string(arg))
        .collect::<io::Result<_>>()?;
    let c_env: Vec<CString> = environment
        .iter()
        .map(|(name, value)| c_string(&format!("{name}={value}")))
        .collect::<io::Result<_>>()?;

    let mut arg_pointers: Vec<*const libc::c_char> = Vec::with_capacity(c_args.len() + 2);
    arg_pointers.push(c_program.as_ptr());
    arg_pointers.extend(c_args.iter().map(|arg| arg.as_ptr()));
    arg_pointers.push(std::ptr::null());

    let mut env_pointers: Vec<*const libc::c_char> = Vec::with_capacity(c_env.len() + 1);
    env_pointers.extend(c_env.iter().map(|entry| entry.as_ptr()));
    env_pointers.push(std::ptr::null());

    // SAFETY: c_program, c_args and c_env all live until the end of this function, so
    // every pointer is valid for the duration of the call, and both lists are
    // NULL-terminated as execve requires. On success execve does not return, so
    // nothing can observe the borrowed pointers afterwards.
    unsafe {
        libc::execve(
            c_program.as_ptr(),
            arg_pointers.as_ptr(),
            env_pointers.as_ptr(),
        )
    };

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
    fn exec_with_env_also_reports_the_program_it_could_not_run() {
        let error = exec_with_env("/nonexistent-program", &[], &[("A", "b")]).unwrap_err();
        assert!(
            error.to_string().contains("/nonexistent-program"),
            "{error}"
        );
    }

    #[test]
    fn a_nul_in_an_environment_value_is_refused_rather_than_truncating_it() {
        // A NUL would end the string early, so PLEX_MEDIA_SERVER_HOME=/a\0/b becomes
        // /a -- a silently different value rather than a rejected one.
        let error = exec_with_env("/bin/true", &[], &[("HOME", "/a\0/b")]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_nul_in_an_argument_is_refused() {
        let error = exec("/bin/sh", &["-c\0evil"]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
