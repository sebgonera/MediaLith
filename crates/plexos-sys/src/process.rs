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

/// A child that has exited and been collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    /// The process that exited.
    pub pid: u32,
    /// Its exit status, if it exited of its own accord.
    pub code: Option<i32>,
    /// The signal that killed it, if one did.
    pub signal: Option<i32>,
}

impl std::fmt::Display for Reaped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(0), _) => write!(f, "pid {} exited normally", self.pid),
            (Some(code), _) => write!(f, "pid {} exited with status {code}", self.pid),
            (_, Some(signal)) => write!(f, "pid {} was killed by signal {signal}", self.pid),
            _ => write!(f, "pid {} stopped for an unrecognised reason", self.pid),
        }
    }
}

/// Collects one exited child, without blocking.
///
/// # Why PID 1 has to call this in a loop
///
/// Every orphaned process on the system is reparented to PID 1 when its own parent dies,
/// and stays a zombie until PID 1 waits for it. A PID 1 that does not reap leaks a process
/// table entry per orphan — and this appliance produces them routinely: `plexosd` spawns
/// `curl`, `ip`, `udhcpc` and the confined Plex child, and a `plexosd` that is restarted
/// leaves all of its behind.
///
/// The exhaustion is slow and the symptom is not "too many zombies": it is `fork` failing
/// somewhere unrelated, weeks later, on a machine nobody has logged into.
///
/// `WNOHANG`, and polled, rather than a `SIGCHLD` handler. A handler is the conventional
/// answer and it costs a signal-safety argument in a codebase whose whole point is that
/// the unsafe is small and reviewable; polling five times a second costs nothing on a
/// machine that is otherwise transcoding video.
///
/// # Errors
/// Only for a `waitpid` failure that is not "nothing to collect". `Ok(None)` covers both
/// "children exist and none has exited" and "there are no children at all", because a
/// supervisor knows which of those it is in and the kernel's answer adds nothing.
pub fn reap() -> io::Result<Option<Reaped>> {
    let mut status: libc::c_int = 0;

    // SAFETY: waitpid writes an int through the pointer, which is a live local for the
    // duration of the call. -1 means "any child of this process", which is the case
    // waitpid is defined for and the only one PID 1 wants; WNOHANG makes it return
    // immediately when there is nothing to collect.
    let pid = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };

    if pid == 0 {
        return Ok(None);
    }
    if pid < 0 {
        let error = io::Error::last_os_error();
        // ECHILD is "you have no children", which is an ordinary state for a supervisor
        // between spawns and not a failure to report.
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(None);
        }
        return Err(error);
    }

    let pid = u32::try_from(pid).unwrap_or(0);

    // Safe functions, not macros wrapping raw reads: they are pure bit arithmetic on the
    // int waitpid filled in, and each is only consulted for the case its predicate
    // reported. Reading the status of a child that was signalled as though it had exited
    // is how a service killed by the OOM killer gets logged as "exited with 0".
    let code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
    let signal = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));

    Ok(Some(Reaped { pid, code, signal }))
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

    #[test]
    fn reaping_with_no_children_is_not_an_error() {
        // ECHILD is the ordinary state of a supervisor between spawns. Reporting it as a
        // failure would make a supervisor log an error five times a second while behaving
        // perfectly correctly.
        assert_eq!(reap().unwrap(), None);
    }

    #[test]
    fn a_child_that_exits_is_collected_with_its_status() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .spawn()
            .expect("a shell to run");
        let pid = child.id();
        // Deliberately leaked rather than waited on: `Child`'s own wait would collect the
        // status and leave nothing for `reap` to find, which is exactly the interaction a
        // supervisor holding `Child` handles has to get right.
        std::mem::forget(child);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reaped = loop {
            if let Some(reaped) = reap().expect("waitpid works") {
                break reaped;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(reaped.pid, pid);
        assert_eq!(reaped.code, Some(3));
        assert_eq!(reaped.signal, None);
        assert!(reaped.to_string().contains("status 3"), "{reaped}");
    }

    #[test]
    fn a_child_that_is_killed_reports_the_signal_rather_than_a_status() {
        // The two have to be told apart: a service killed by the OOM killer and one that
        // exited with a code are different problems, and a supervisor that logged "exited
        // with 0" for the first would hide it completely.
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "kill -9 $$"])
            .spawn()
            .expect("a shell to run");
        std::mem::forget(child);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reaped = loop {
            if let Some(reaped) = reap().expect("waitpid works") {
                break reaped;
            }
            assert!(std::time::Instant::now() < deadline, "the child never died");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(reaped.code, None);
        assert_eq!(reaped.signal, Some(libc::SIGKILL));
        assert!(reaped.to_string().contains("killed by signal"), "{reaped}");
    }
}
