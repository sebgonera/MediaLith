//! The console's terminal session (ADR-0014).
//!
//! One shell, one session, reachable from the page. Everything about the shape of this
//! module comes from two facts: the HTTP server hands handlers a finished `Vec<u8>` and
//! cannot stream, and it runs a thread per connection so a handler may block.
//!
//! # How output gets out
//!
//! A reader thread drains the PTY master into a ring buffer that counts every byte it has
//! ever seen. Clients ask for "everything after byte N" and block until there is
//! something or a deadline passes. The offset is what makes it correct: a client asking
//! "what is new" loses whatever arrived between two polls, and a client asking for
//! everything after a position it already has cannot.
//!
//! The buffer is bounded, so a program that floods the terminal cannot exhaust memory.
//! When it drops output it says so rather than silently renumbering, because a terminal
//! that quietly skips bytes produces a scrollback that never happened.
//!
//! # Why one session
//!
//! Two browsers sharing one PTY is a feature nobody asked for and a confusing one to
//! debug. A second request is refused with a reason and the option to take over.
//!
//! # The idle timeout is a safety property
//!
//! A browser tab closed without ceremony leaves a root shell running. Nothing in a
//! request/response server notices that promptly, so the session ends on its own if
//! nobody has asked for output in a while.
//!
//! # What has run
//!
//! **Nothing on hardware.** Exercised by tests on a build host.

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use plexos_sys::pty::{self, Terminal, WindowSize};

/// The shell a session runs.
///
/// Absolute, because `plexosd` inherits an empty environment from PID 1 and `execvp`'s
/// fallback of `/bin:/usr/bin` has already cost this project a boot. `/bin/sh` is
/// busybox's shell here, which is what an administrator gets on the attached screen too —
/// the terminal should not be a different environment from the one the documentation
/// describes.
pub const SHELL: &str = "/bin/sh";

/// How much output is kept for a client that has fallen behind.
///
/// 256 KiB is a few thousand lines. Beyond that the oldest bytes go, and the client is
/// told they went.
pub const BUFFER_BYTES: usize = 256 * 1024;

/// How long a poll waits before returning empty-handed.
///
/// Comfortably below [`crate::http::IO_TIMEOUT`], because a handler that outlasts the
/// socket's write timeout produces a broken response rather than an empty one.
pub const POLL_WAIT: Duration = Duration::from_secs(10);

/// How long a session survives with nobody reading it.
///
/// A closed browser tab leaves a root shell running otherwise. Long enough to survive a
/// page reload or a laptop lid, short enough that a forgotten session is not a permanent
/// one.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Output buffered for clients, and how much has been lost.
#[derive(Debug, Default)]
struct Buffer {
    /// The bytes still held.
    bytes: Vec<u8>,
    /// Offset of `bytes[0]` in the session's whole output.
    base: u64,
    /// Whether the shell has finished.
    finished: bool,
}

impl Buffer {
    /// Total bytes ever produced.
    fn end(&self) -> u64 {
        self.base + self.bytes.len() as u64
    }

    /// Appends, dropping the oldest when the bound is reached.
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > BUFFER_BYTES {
            let excess = self.bytes.len() - BUFFER_BYTES;
            self.bytes.drain(..excess);
            self.base += excess as u64;
        }
    }

    /// Everything after `since`, and where that leaves the caller.
    fn since(&self, since: u64) -> (Vec<u8>, u64, bool) {
        // A caller behind the window gets what is left and is told bytes were lost, rather
        // than being silently renumbered into a scrollback that never happened.
        let lost = since < self.base;
        let from = since.max(self.base);
        let offset = usize::try_from(from - self.base).unwrap_or(self.bytes.len());
        let offset = offset.min(self.bytes.len());
        (self.bytes[offset..].to_vec(), self.end(), lost)
    }
}

/// A running shell and everything the routes need to talk to it.
#[derive(Debug)]
struct Session {
    id: String,
    terminal: Terminal,
    buffer: Arc<(Mutex<Buffer>, Condvar)>,
    last_polled: Instant,
}

/// The one session, if there is one.
static CURRENT: std::sync::OnceLock<Mutex<Option<Session>>> = std::sync::OnceLock::new();

fn current() -> MutexGuard<'static, Option<Session>> {
    CURRENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a caller gets back when a session is opened.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Opened {
    /// The identifier every later request must carry.
    pub id: String,
    /// Where its output starts, which is always zero for a new session.
    pub offset: u64,
}

/// What a poll returns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Output {
    /// Bytes produced since the requested offset, as UTF-8 with replacements.
    ///
    /// Lossy on purpose. A terminal emits escape sequences and, mid-stream, half of a
    /// multi-byte character; refusing to answer because a byte boundary fell awkwardly
    /// would stall the session on ordinary output.
    pub data: String,
    /// The offset to ask for next time.
    pub offset: u64,
    /// Whether output was dropped before what is being returned.
    pub lost: bool,
    /// Whether the shell has exited.
    pub finished: bool,
}

/// Opens a session, replacing an existing one only when asked.
///
/// # Errors
/// If a session is already running and `take_over` is false, or if the shell cannot be
/// started.
pub fn open(size: WindowSize, take_over: bool) -> Result<Opened, String> {
    let mut slot = current();

    if let Some(existing) = slot.as_ref() {
        if !take_over && !is_finished(existing) {
            return Err(format!(
                "a terminal session is already open ({}). Remedy: close it, or ask again \
                 with take_over so this one replaces it. Two browsers sharing one shell \
                 is a confusing thing to debug and nobody asked for it.",
                existing.id
            ));
        }
        end(slot.take());
    }

    // Not a token, and it does not need to be: the session is already behind the ADR-0013
    // gate, and this only distinguishes one session from a stale request for a previous
    // one. Derived from the PTY's own identity so it is unique without a random source.
    let terminal = pty::spawn(SHELL, &["sh", "-l"], size)
        .map_err(|e| format!("could not start {SHELL}: {e}"))?;
    let id = format!("s{}", terminal.pid);

    let buffer = Arc::new((Mutex::new(Buffer::default()), Condvar::new()));
    spawn_reader(&terminal, &buffer)?;

    *slot = Some(Session {
        id: id.clone(),
        terminal,
        buffer,
        last_polled: Instant::now(),
    });

    Ok(Opened { id, offset: 0 })
}

/// Drains the master into the buffer until end of file.
///
/// Its own thread because the read blocks, and a blocked handler would hold the one
/// session lock for as long as the shell stayed quiet.
fn spawn_reader(terminal: &Terminal, buffer: &Arc<(Mutex<Buffer>, Condvar)>) -> Result<(), String> {
    let master = terminal
        .master
        .try_clone()
        .map_err(|e| format!("could not duplicate the terminal descriptor: {e}"))?;
    let buffer = Arc::clone(buffer);

    std::thread::spawn(move || {
        let mut file = std::fs::File::from(master);
        let mut chunk = [0u8; 8192];
        loop {
            match file.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let (lock, signal) = &*buffer;
                    lock.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(&chunk[..n]);
                    signal.notify_all();
                }
            }
        }

        let (lock, signal) = &*buffer;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finished = true;
        signal.notify_all();
    });

    Ok(())
}

/// Waits for output after `since`, up to [`POLL_WAIT`].
///
/// # Errors
/// If `id` is not the running session, which is how a client learns its session went away
/// rather than sitting in a loop that never produces anything.
pub fn poll(id: &str, since: u64) -> Result<Output, String> {
    let buffer = {
        let mut slot = current();
        let session = slot
            .as_mut()
            .filter(|s| s.id == id)
            .ok_or_else(|| no_such_session(id))?;
        session.last_polled = Instant::now();
        Arc::clone(&session.buffer)
    };

    // The session lock is released before waiting. Holding it here would block every other
    // route -- including the one that closes this session -- for the whole poll.
    let (lock, signal) = &*buffer;
    let mut held = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let deadline = Instant::now() + POLL_WAIT;
    while held.end() <= since && !held.finished {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let (guard, timeout) = signal
            .wait_timeout(held, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held = guard;
        if timeout.timed_out() {
            break;
        }
    }

    let (data, offset, lost) = held.since(since);
    Ok(Output {
        data: String::from_utf8_lossy(&data).into_owned(),
        offset,
        lost,
        finished: held.finished,
    })
}

/// Sends keystrokes to the shell.
///
/// # Errors
/// If `id` is not the running session, or the write fails.
pub fn input(id: &str, bytes: &[u8]) -> Result<(), String> {
    let mut slot = current();
    let session = slot
        .as_mut()
        .filter(|s| s.id == id)
        .ok_or_else(|| no_such_session(id))?;

    session.last_polled = Instant::now();

    let master = session
        .terminal
        .master
        .try_clone()
        .map_err(|e| format!("could not duplicate the terminal descriptor: {e}"))?;

    std::fs::File::from(master)
        .write_all(bytes)
        .map_err(|e| format!("could not write to the shell: {e}"))
}

/// Tells the shell how big the window is.
///
/// # Errors
/// If `id` is not the running session.
pub fn resize(id: &str, size: WindowSize) -> Result<(), String> {
    let slot = current();
    let session = slot
        .as_ref()
        .filter(|s| s.id == id)
        .ok_or_else(|| no_such_session(id))?;

    pty::set_window_size(&session.terminal, size)
        .map_err(|e| format!("could not set the window size: {e}"))
}

/// Ends the session, if `id` names it.
pub fn close(id: &str) {
    let mut slot = current();
    if slot.as_ref().is_some_and(|s| s.id == id) {
        end(slot.take());
    }
}

/// Ends a session whose client has gone away.
///
/// Called from the routes rather than a timer thread: this daemon has enough threads, and
/// a session nobody is polling is a session nobody will notice the closing of. The one
/// case it does not cover — a tab closed while no other request ever arrives — is covered
/// by the next request of any kind.
pub fn expire_if_idle() {
    let mut slot = current();
    let idle = slot
        .as_ref()
        .is_some_and(|s| s.last_polled.elapsed() > IDLE_TIMEOUT);
    if idle {
        end(slot.take());
    }
}

/// Whether a session is running, for the page to render against.
#[must_use]
pub fn is_open() -> bool {
    current().as_ref().is_some_and(|s| !is_finished(s))
}

fn is_finished(session: &Session) -> bool {
    session
        .buffer
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finished
}

/// Hangs up the shell and reaps it.
fn end(session: Option<Session>) {
    let Some(session) = session else { return };

    pty::close(&session.terminal);

    // Reaped rather than left: plexosd never exits, so an unreaped child is a zombie for
    // the life of the daemon. Bounded, because a shell that ignores SIGHUP must not hold
    // up the request that asked for this.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if pty::try_reap(&session.terminal).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn no_such_session(id: &str) -> String {
    format!(
        "there is no terminal session {id}. Remedy: it timed out, the shell exited, or \
         another browser took it over. Open a new one."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sessions are global, so tests that open one must not run beside each other.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn wait_for(id: &str, needle: &str) -> String {
        let mut seen = String::new();
        let mut offset = 0;
        let deadline = Instant::now() + Duration::from_secs(5);

        while Instant::now() < deadline {
            let out = poll(id, offset).expect("the session is open");
            seen.push_str(&out.data);
            offset = out.offset;
            if seen.contains(needle) || out.finished {
                break;
            }
        }
        seen
    }

    #[test]
    fn a_session_runs_a_shell_and_its_output_comes_back() {
        let _guard = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let opened = open(WindowSize::default(), true).expect("opens");
        input(&opened.id, b"echo terminal-works\n").expect("writes");

        let seen = wait_for(&opened.id, "terminal-works");
        assert!(seen.contains("terminal-works"), "got {seen:?}");

        close(&opened.id);
        assert!(!is_open());
    }

    #[test]
    fn a_second_session_is_refused_unless_it_asks_to_take_over() {
        let _guard = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let first = open(WindowSize::default(), true).expect("opens");

        let error = open(WindowSize::default(), false).expect_err("must refuse");
        assert!(error.contains("Remedy:"), "{error}");
        assert!(error.contains("take_over"), "{error}");

        let second = open(WindowSize::default(), true).expect("takes over");
        assert_ne!(first.id, second.id);

        close(&second.id);
    }

    #[test]
    fn a_request_for_a_session_that_is_gone_says_so_rather_than_hanging() {
        // The alternative is a browser polling forever against nothing, which looks
        // exactly like a shell that has stopped producing output.
        let _guard = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let error = poll("s0", 0).expect_err("no such session");
        assert!(error.contains("Remedy:"), "{error}");
        assert!(input("s0", b"x").is_err());
    }

    #[test]
    fn output_is_addressed_by_offset_so_nothing_between_polls_is_lost() {
        // The property that makes long-polling correct rather than merely convenient.
        let mut buffer = Buffer::default();
        buffer.push(b"hello ");
        let (first, offset, _) = buffer.since(0);
        assert_eq!(first, b"hello ");

        buffer.push(b"world");
        let (second, _, lost) = buffer.since(offset);
        assert_eq!(second, b"world", "exactly what arrived after the last poll");
        assert!(!lost);
    }

    #[test]
    fn a_client_that_falls_behind_the_window_is_told_bytes_were_lost() {
        // Silently renumbering would give a scrollback that never happened.
        let mut buffer = Buffer::default();
        buffer.push(&vec![b'x'; BUFFER_BYTES + 1024]);

        let (data, _, lost) = buffer.since(0);
        assert!(lost, "the beginning is gone and the caller must know");
        assert_eq!(data.len(), BUFFER_BYTES);
        assert_eq!(buffer.base, 1024);
    }

    #[test]
    fn the_buffer_is_bounded_so_a_flooding_program_cannot_exhaust_memory() {
        let mut buffer = Buffer::default();
        for _ in 0..64 {
            buffer.push(&vec![b'y'; 32 * 1024]);
        }
        assert_eq!(buffer.bytes.len(), BUFFER_BYTES);
        assert_eq!(buffer.end(), 64 * 32 * 1024);
    }

    #[test]
    fn a_poll_waits_for_output_rather_than_returning_immediately_empty() {
        // What replaces a streaming response. A poll that returned at once would make the
        // page a busy loop and the output arrive a fixed delay late.
        let _guard = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let opened = open(WindowSize::default(), true).expect("opens");
        let first = poll(&opened.id, 0).expect("polls");

        let started = Instant::now();
        std::thread::spawn({
            let id = opened.id.clone();
            move || {
                std::thread::sleep(Duration::from_millis(300));
                let _ = input(&id, b"echo late-output\n");
            }
        });

        let out = poll(&opened.id, first.offset).expect("polls");
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "it must have waited rather than returned empty"
        );
        assert!(!out.data.is_empty(), "and come back with the output");

        close(&opened.id);
    }

    #[test]
    fn the_poll_wait_is_shorter_than_the_sockets_write_timeout() {
        // A handler that outlasts IO_TIMEOUT produces a broken response rather than an
        // empty one, and the browser cannot tell those apart.
        assert!(
            POLL_WAIT < crate::http::IO_TIMEOUT,
            "a poll that outlives the socket is a bug that looks like a network fault"
        );
    }

    #[test]
    fn the_idle_timeout_is_long_enough_to_survive_a_reload_and_short_enough_to_matter() {
        // A safety property, not a nicety: a closed tab otherwise leaves a root shell.
        assert!(IDLE_TIMEOUT >= Duration::from_secs(60));
        assert!(IDLE_TIMEOUT <= Duration::from_secs(3600));
    }
}
