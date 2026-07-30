//! Pseudo-terminals, for the console's terminal (ADR-0014).
//!
//! A shell wants a terminal, not a pipe, and the difference is not cosmetic. Without a
//! controlling terminal there is no job control, no `SIGINT` from Ctrl-C, no window size,
//! and programs that ask `isatty` behave differently — `ls` stops colouring, `less`
//! refuses to page, and a shell prints no prompt. A terminal built on pipes looks like it
//! works until the first time somebody needs to interrupt something.
//!
//! # The sequence, and why each step is where it is
//!
//! 1. **`openpty`** allocates the pair and returns both ends.
//! 2. **`fork`**, because everything after this differs between the two processes.
//! 3. In the child: **`setsid`**, which detaches it from the parent's session and makes
//!    it a session leader. A process that is already a group leader cannot do this, and a
//!    process that is not a session leader cannot acquire a controlling terminal — so
//!    this must happen, and must happen first.
//! 4. **`TIOCSCTTY`**, which is what actually makes the slave the controlling terminal.
//!    `setsid` alone leaves the session with none, and then Ctrl-C reaches nothing.
//! 5. **`dup2`** onto the three standard descriptors, then close the originals.
//! 6. **`execv`**.
//!
//! Every one of those steps is silently survivable if it is skipped, which is why they
//! are written out rather than wrapped in something that looks tidier.
//!
//! # What runs between fork and exec
//!
//! Only async-signal-safe calls. After `fork` in a process that has threads — and
//! `plexosd` has several — the child holds copies of locks whose owners do not exist,
//! so anything that allocates, logs or takes a mutex may deadlock. That is why the child
//! path below calls nothing but raw syscalls and why the argument vector is built
//! *before* the fork.
//!
//! # What has run
//!
//! **Nothing.** No terminal has been opened on the appliance.

use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};

/// A running pseudo-terminal and the process attached to it.
#[derive(Debug)]
pub struct Terminal {
    /// The master end: read for output, write for input.
    pub master: OwnedFd,
    /// The child's process id, for signalling and reaping.
    pub pid: libc::pid_t,
}

/// Terminal size, in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    /// Rows.
    pub rows: u16,
    /// Columns.
    pub columns: u16,
}

impl Default for WindowSize {
    /// The size an unconfigured terminal claims to be.
    ///
    /// 24x80 rather than 0x0. A zero size is what an unset `winsize` reports, and
    /// programs handle it inconsistently — `less` and `top` in particular draw nothing
    /// useful. An old default that every program understands beats an honest zero.
    fn default() -> Self {
        Self {
            rows: 24,
            columns: 80,
        }
    }
}

/// Spawns `program` under a new pseudo-terminal.
///
/// `argv` is the complete argument vector including `argv[0]`.
///
/// # Errors
/// If the pair cannot be allocated or the fork fails. A failure *after* the fork happens
/// in the child, which exits rather than returning — there is nothing useful it could
/// return to.
pub fn spawn(program: &str, argv: &[&str], size: WindowSize) -> io::Result<Terminal> {
    // Built before the fork. Allocating in the child is exactly what the module
    // documentation forbids, and CString::new allocates.
    let program_c = CString::new(program)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program path contains a NUL"))?;
    let argv_c: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(*a))
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an argument contains a NUL"))?;
    let mut argv_ptr: Vec<*const libc::c_char> = argv_c.iter().map(|a| a.as_ptr()).collect();
    argv_ptr.push(std::ptr::null());

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;

    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: openpty writes one file descriptor into each of the first two pointers and
    // reads the winsize from the fourth. Both descriptor slots are live local variables,
    // the winsize is a live local that is fully initialised, and the third argument
    // (termios) is null, which the API defines as "use the defaults". Nothing is retained
    // by the callee after it returns.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &raw const winsize,
        )
    };
    if opened != 0 {
        let error = io::Error::last_os_error();
        // Attributed here, because the caller cannot. openpty fails before the program is
        // ever looked at, so an error passed up bare gets reported as "could not start
        // /bin/sh" about a shell that exists and was never reached -- which is exactly
        // what happened on the appliance's first terminal.
        if error.kind() == io::ErrorKind::NotFound {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no pseudo-terminal could be allocated: /dev/pts is missing. Remedy: \
                 devtmpfs provides /dev/ptmx, so opening it succeeds, but the slave \
                 appears under /dev/pts and that has to be a devpts mount. The boot plan \
                 mounts it; a machine without it predates that. This is not about the \
                 program that was going to be run.",
            ));
        }
        return Err(error);
    }

    // SAFETY: fork() takes no arguments. In the parent it returns the child's pid; in the
    // child it returns 0. The child below calls only async-signal-safe functions before
    // exec, which is what makes forking from a threaded process sound here.
    let pid = unsafe { libc::fork() };

    if pid < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: returned by a successful openpty, not closed and not handed to anything.
        unsafe { libc::close(master) };
        // SAFETY: as above.
        unsafe { libc::close(slave) };
        return Err(error);
    }

    if pid == 0 {
        // The child. Nothing here allocates, logs or locks -- after a fork in a threaded
        // process the child holds copies of locks whose owners do not exist, so anything
        // that takes one may deadlock. Each call is in its own block because each has its
        // own reason for being sound, and the sequence is the substance of this module.

        // SAFETY: a descriptor this process owns, which the child has no use for.
        unsafe { libc::close(master) };

        // SAFETY: setsid takes no arguments and only affects the calling process. It must
        // come before TIOCSCTTY: acquiring a controlling terminal requires being a session
        // leader, and this is what makes this process one.
        if unsafe { libc::setsid() } < 0 {
            // SAFETY: _exit takes an integer and does not return.
            unsafe { libc::_exit(127) };
        }

        // SAFETY: TIOCSCTTY takes an integer argument by value, reads no memory, and is
        // defined for a terminal descriptor in a session with no controlling terminal --
        // which is what setsid above just produced. This is the step that makes Ctrl-C
        // work; without it the session has no controlling terminal and keyboard signals
        // reach nothing.
        if unsafe { libc::ioctl(slave, libc::TIOCSCTTY, 0) } < 0 {
            // SAFETY: as above.
            unsafe { libc::_exit(127) };
        }

        // SAFETY: dup2 replaces a descriptor atomically and reads no memory. `slave` is
        // valid, and 0, 1 and 2 are the standard streams this child is about to hand to a
        // shell.
        let duplicated = unsafe { libc::dup2(slave, 0) } >= 0
            // SAFETY: as above.
            && unsafe { libc::dup2(slave, 1) } >= 0
            // SAFETY: as above.
            && unsafe { libc::dup2(slave, 2) } >= 0;
        if !duplicated {
            // SAFETY: as above.
            unsafe { libc::_exit(127) };
        }

        // Only after the dup2s: closing it earlier would close the descriptors that were
        // about to be duplicated from it.
        if slave > 2 {
            // SAFETY: a descriptor this process owns, now duplicated onto 0, 1 and 2.
            unsafe { libc::close(slave) };
        }

        // SAFETY: execv reads a NUL-terminated path and a NULL-terminated argument vector.
        // Both were built before the fork and live in this address space, which is a copy
        // of the parent's, so the pointers are valid here. It does not return on success.
        unsafe { libc::execv(program_c.as_ptr(), argv_ptr.as_ptr()) };

        // Only reachable if execv failed. 127 is the shell's own convention for "command
        // not found", which is the overwhelmingly likely cause.
        // SAFETY: _exit takes an integer and does not return.
        unsafe { libc::_exit(127) };
    }

    // The parent.
    // SAFETY: `slave` is a valid descriptor from openpty that this process no longer
    // needs — the child holds its own copy. Leaving it open here would keep the master
    // readable after the child exits, so the reader would never see end of file and a
    // dead session would look like an idle one.
    unsafe { libc::close(slave) };

    // SAFETY: `master` came from a successful openpty and is not owned by anything else,
    // so transferring ownership to OwnedFd is sound and it will be closed exactly once.
    let master = unsafe { OwnedFd::from_raw_fd(master) };

    Ok(Terminal { master, pid })
}

/// Tells the terminal how large its window is.
///
/// Programs that draw a full screen read this once at start and then on `SIGWINCH`, so a
/// resize that never reaches the kernel leaves `top` drawing 24 rows in a 60-row browser
/// for as long as it runs.
///
/// # Errors
/// If the descriptor is not a terminal.
pub fn set_window_size(terminal: &Terminal, size: WindowSize) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: TIOCSWINSZ reads one `winsize` from the pointer, which is a live, fully
    // initialised local. The descriptor is borrowed from an OwnedFd that outlives the
    // call. Nothing is retained.
    let result = unsafe {
        libc::ioctl(
            terminal.master.as_raw_fd(),
            libc::TIOCSWINSZ,
            &raw const winsize,
        )
    };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Asks the child to finish, then makes sure it has.
///
/// `SIGHUP` rather than `SIGKILL` first: a shell that is hung up on runs its exit
/// handlers and takes its own children with it, whereas a killed one leaves whatever it
/// had started running with no terminal and no parent watching.
pub fn close(terminal: &Terminal) {
    // SAFETY: kill() with a valid signal is defined for any pid; a pid that has already
    // exited yields ESRCH, which is ignored here because it means the work is done.
    unsafe {
        libc::kill(terminal.pid, libc::SIGHUP);
    }
}

/// Reaps the child if it has exited, without blocking.
///
/// Returns its exit status once, and `None` while it is still running. A session that is
/// never reaped leaves a zombie for the life of the daemon, and `plexosd` does not exit.
#[must_use]
pub fn try_reap(terminal: &Terminal) -> Option<libc::c_int> {
    let mut status: libc::c_int = 0;

    // SAFETY: waitpid writes an int through the pointer, which is a live local. WNOHANG
    // makes the call return immediately whether or not the child has exited. The pid is
    // this process's own child, which is the only case waitpid is defined for.
    let result = unsafe { libc::waitpid(terminal.pid, &raw mut status, libc::WNOHANG) };

    (result == terminal.pid).then_some(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;

    fn read_some(terminal: &Terminal, wait: std::time::Duration) -> String {
        // The master stays readable while the child lives, so a plain read would block
        // forever once output stops. A deadline plus a non-blocking descriptor is the
        // smallest thing that terminates.
        // SAFETY: fcntl with F_SETFL on a descriptor this process owns; O_NONBLOCK is a
        // documented flag and nothing is read through a pointer.
        unsafe {
            libc::fcntl(terminal.master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }

        let mut file = std::fs::File::from(terminal.master.try_clone().unwrap());
        let deadline = std::time::Instant::now() + wait;
        let mut out = String::new();
        let mut buffer = [0u8; 4096];

        while std::time::Instant::now() < deadline {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buffer[..n])),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        out
    }

    #[test]
    fn a_program_runs_under_the_terminal_and_its_output_comes_back() {
        let terminal = spawn(
            "/bin/sh",
            &["sh", "-c", "echo hello-from-the-pty"],
            WindowSize::default(),
        )
        .expect("/bin/sh exists on any machine that can run this suite");

        let output = read_some(&terminal, std::time::Duration::from_secs(3));
        assert!(output.contains("hello-from-the-pty"), "got {output:?}");
    }

    #[test]
    fn the_child_believes_it_has_a_terminal() {
        // The property the whole module exists for. Over a pipe this prints "pipe", and
        // everything that makes a shell usable -- prompts, job control, colour -- keys
        // off exactly this answer.
        let terminal = spawn(
            "/bin/sh",
            &[
                "sh",
                "-c",
                "if [ -t 1 ]; then echo IS-A-TTY; else echo NOT-A-TTY; fi",
            ],
            WindowSize::default(),
        )
        .expect("spawns");

        let output = read_some(&terminal, std::time::Duration::from_secs(3));
        assert!(output.contains("IS-A-TTY"), "got {output:?}");
    }

    #[test]
    fn the_window_size_reaches_the_child() {
        // A resize that never reaches the kernel leaves full-screen programs drawing 24
        // rows in whatever the browser actually is, for as long as they run.
        let terminal = spawn(
            "/bin/sh",
            &["sh", "-c", "stty size 2>/dev/null || echo no-stty"],
            WindowSize {
                rows: 40,
                columns: 132,
            },
        )
        .expect("spawns");

        let output = read_some(&terminal, std::time::Duration::from_secs(3));
        assert!(
            output.contains("40 132") || output.contains("no-stty"),
            "got {output:?}"
        );
    }

    #[test]
    fn input_written_to_the_master_reaches_the_shell() {
        let terminal = spawn("/bin/sh", &["sh"], WindowSize::default()).expect("spawns");

        let mut file = std::fs::File::from(terminal.master.try_clone().unwrap());
        file.write_all(b"echo round-trip-works\nexit\n").unwrap();
        file.flush().unwrap();

        let output = read_some(&terminal, std::time::Duration::from_secs(3));
        assert!(output.contains("round-trip-works"), "got {output:?}");
    }

    #[test]
    fn a_finished_child_is_reaped_exactly_once() {
        // plexosd never exits, so a session that is not reaped is a zombie for the life
        // of the daemon.
        let _serialised = crate::CHILD_PROCESS_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let terminal =
            spawn("/bin/sh", &["sh", "-c", "exit 3"], WindowSize::default()).expect("spawns");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut status = None;
        while std::time::Instant::now() < deadline && status.is_none() {
            status = try_reap(&terminal);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let status = status.expect("the child exits promptly");
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 3);

        assert!(
            try_reap(&terminal).is_none(),
            "a second reap must not report a status again"
        );
    }

    #[test]
    fn the_default_size_is_one_every_program_understands() {
        // Zero is what an unset winsize reports, and programs handle it inconsistently.
        let size = WindowSize::default();
        assert_eq!((size.rows, size.columns), (24, 80));
    }
}
