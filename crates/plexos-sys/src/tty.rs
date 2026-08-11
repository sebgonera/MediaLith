//! The virtual terminal, as something to draw on rather than to log to.
//!
//! Everything that has ever written to this machine's attached screen has written *lines*:
//! PID 1's log, the health gate's verdict, the banner that prints the device token. A line
//! needs nothing from the terminal beyond a file descriptor. A screen does — it needs to
//! know how big it is, and it needs the keyboard to arrive a keystroke at a time rather
//! than a line at a time, which is a mode the terminal is in only if something asks.
//!
//! Two ioctls and two `termios` calls, which is the whole of it. `stty` would do the same
//! job and is not in this image; `setterm` is not either, which is already recorded in
//! `plexos-init`'s screen module as the reason it writes escape sequences by hand.
//!
//! # Why this is here and not in `plexosd`
//!
//! `unsafe_code` is forbidden everywhere else, and reading a `struct winsize` out of an
//! ioctl is unsafe by construction. The rule the workspace follows is that the answer to
//! wanting `unsafe` elsewhere is a function in this crate, so this is that function.
//!
//! # Restoring is the caller's job, and it matters
//!
//! A terminal left in raw mode is a terminal where the shell that comes after shows no
//! typing and needs no Enter. [`raw`] therefore returns what the settings *were*, and the
//! caller is expected to put them back. On this appliance the dashboard owns `/dev/tty1`
//! for the life of the machine and there is nothing after it — but "nothing after it" is a
//! property of today's arrangement, and a function that made it permanent would be a trap
//! for whoever changes that arrangement.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// A terminal's size in character cells.
///
/// Rows and columns rather than pixels, because that is what a terminal answers and what
/// anything drawing on one has to think in. On the reference laptop this is 101 by 360:
/// a 2880x1620 panel with an 8x16 font, which is *not* what the kernel command line asks
/// for — it requests `video=1280x720` and `fbcon=font:TER16x32`, and i915 takes the
/// console over and drives the panel at its native mode regardless. Anything that assumed
/// the command line's numbers would be laying out for a screen this machine does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Character rows.
    pub rows: u16,
    /// Character columns.
    pub columns: u16,
}

/// Asks the terminal how big it is.
///
/// # Errors
/// If the file is not a terminal, which is the ordinary answer for a pipe or a file and
/// is why the caller must have somewhere sensible to fall back to.
pub fn size(terminal: &File) -> io::Result<Size> {
    let mut winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: `terminal` is an open file descriptor for the lifetime of this call, and
    // TIOCGWINSZ writes a `struct winsize` through the pointer — which is exactly the type
    // of the local it is given, and the local outlives the call.
    let result = unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCGWINSZ, &raw mut winsize) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(Size {
        rows: winsize.ws_row,
        columns: winsize.ws_col,
    })
}

/// A terminal's settings, kept so they can be put back.
///
/// Opaque on purpose: it exists to be handed to [`restore`] and there is nothing in it a
/// caller has any business reading.
#[derive(Clone)]
pub struct Settings(libc::termios);

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The contents are flag words nobody reads, and printing them would put a wall of
        // integers into any log line that happens to include the struct holding this.
        f.write_str("Settings(the terminal's previous mode)")
    }
}

/// Puts the terminal into the mode a screen needs, and returns the mode it was in.
///
/// What changes and why, because "raw mode" is a name for a set of decisions rather than
/// one thing:
///
/// - **`ICANON` off** so a read returns as soon as a key is pressed. With it on, the
///   terminal buffers a line and hands it over at Enter — so `P` would do nothing until
///   somebody pressed Return after it, which reads as a dashboard that ignores the
///   keyboard.
/// - **`ECHO` off** so the keys somebody presses are not painted over the screen being
///   drawn. This is the difference between a dashboard and a dashboard with a stray `p`
///   in the middle of it.
/// - **`ISIG` off** so Ctrl+C is a keystroke rather than a signal. On this appliance the
///   dashboard's terminal is not its controlling terminal, so the signal would go nowhere
///   anyway — turning it off means that stays true if something ever gives it one.
/// - **`IXON` off** so Ctrl+S does not silently stop the screen updating. That failure
///   looks exactly like a frozen appliance and is undone by a keystroke nobody knows to
///   press.
/// - **`OPOST` left alone**, which is a decision and not an omission. Turning it off is
///   the textbook half of "raw mode" and would be wrong here: this terminal is not solely
///   the caller's. PID 1 writes its service lines to `/dev/console`, which is the
///   foreground virtual terminal, which is the one the dashboard draws on — and without
///   `ONLCR` those lines staircase down the screen, each starting where the last ended.
///   A drawing that positions its own cursor does not care either way, so the setting that
///   helps somebody else and costs nothing is the one to keep.
/// - **`VMIN` 1, `VTIME` 0** so a read blocks until exactly one key arrives. The reader is
///   a thread of its own, so blocking is what it is for; a timeout here would be a poll
///   loop burning a core to find out that nobody is standing at the machine.
///
/// # Errors
/// If the file is not a terminal, or the settings cannot be applied.
pub fn raw(terminal: &File) -> io::Result<Settings> {
    let fd = terminal.as_raw_fd();
    let mut current = std::mem::MaybeUninit::<libc::termios>::uninit();

    // SAFETY: tcgetattr fills the `struct termios` behind the pointer and returns < 0
    // without writing anything if it fails. The local is the right type and outlives the
    // call; it is read below only on success, which is when it has been initialised.
    if unsafe { libc::tcgetattr(fd, current.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: tcgetattr returned success above, so the value is initialised.
    let previous = unsafe { current.assume_init() };

    let mut raw = previous;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
    raw.c_lflag &= !(libc::ISIG | libc::IEXTEN);
    raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR | libc::IGNCR | libc::ISTRIP);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;

    // SAFETY: `fd` is open for the lifetime of this call and `raw` is a fully initialised
    // `struct termios` that tcsetattr only reads.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const raw) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(Settings(previous))
}

/// Puts a terminal back the way [`raw`] found it.
///
/// # Errors
/// If the settings cannot be applied, which for a terminal that has gone away is expected
/// and is why every caller here treats it as a comfort rather than a step that can fail.
pub fn restore(terminal: &File, settings: &Settings) -> io::Result<()> {
    // SAFETY: the descriptor is open for the lifetime of this call, and `settings.0` is a
    // `struct termios` obtained from tcgetattr, which tcsetattr only reads.
    if unsafe { libc::tcsetattr(terminal.as_raw_fd(), libc::TCSANOW, &raw const settings.0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build host has no `/dev/tty1` to borrow and must not touch one if it does, so
    /// these run against a PTY — which is a terminal in every way that matters here.
    ///
    /// **Both halves are returned**, and the first version of this helper closed the
    /// controlling half immediately and kept only the device. That destroys the pair: the
    /// device survives as a descriptor and stops being a terminal, so `tcgetattr` answered
    /// `EIO` and two tests failed in a way that read as the ioctls being wrong rather than
    /// the fixture being wrong.
    fn a_terminal() -> Option<(File, File)> {
        use std::os::fd::{FromRawFd as _, OwnedFd};

        let mut controller = 0;
        let mut device = 0;
        // SAFETY: openpty writes two descriptors through the first two pointers and takes
        // null for the three optional arguments, which is documented as "use the
        // defaults". Both locals outlive the call.
        let opened = unsafe {
            libc::openpty(
                &raw mut controller,
                &raw mut device,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if opened < 0 {
            return None;
        }
        // SAFETY: both are freshly opened descriptors this test now owns, and each is
        // wrapped exactly once, so each is closed exactly once when its File is dropped.
        let device = unsafe { OwnedFd::from_raw_fd(device) };
        // SAFETY: likewise for the controlling half. It is returned rather than dropped:
        // closing it would take the pair down with it.
        let controller = unsafe { OwnedFd::from_raw_fd(controller) };
        Some((File::from(device), File::from(controller)))
    }

    #[test]
    fn a_terminal_reports_a_size() {
        let Some((terminal, _controller)) = a_terminal() else {
            println!("skip: no pty available on this host");
            return;
        };
        // A fresh pty is 0x0 until something sets it, so what is asserted is that the
        // question was answered at all -- which is the failure mode that matters, because
        // the caller's fallback is what gets used when this errors.
        assert!(size(&terminal).is_ok());
    }

    #[test]
    fn something_that_is_not_a_terminal_says_so_rather_than_guessing() {
        // The path that decides whether the dashboard draws or reports that it cannot.
        // Named for this test: Rust runs tests as threads in one process, so a fixed path
        // is one test deleting what another is reading.
        let path = std::env::temp_dir().join("plexos-tty-not-a-terminal-test");
        let file = File::create(&path).expect("a scratch file");
        assert!(size(&file).is_err());
        assert!(raw(&file).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn raw_mode_turns_off_the_line_discipline_and_restoring_puts_it_back() {
        // The property, read back through the same interface rather than assumed from the
        // flags written -- a mask applied to the wrong field is the mistake here, and it
        // would leave a dashboard that only reacts to a key after somebody presses Enter.
        let Some((terminal, _controller)) = a_terminal() else {
            println!("skip: no pty available on this host");
            return;
        };

        let before = current_lflag(&terminal);
        assert!(before & libc::ICANON != 0, "a fresh pty is line-buffered");

        let saved = raw(&terminal).expect("a pty accepts termios");
        let during = current_lflag(&terminal);
        assert_eq!(during & libc::ICANON, 0, "a key arrives without Enter");
        assert_eq!(during & libc::ECHO, 0, "and is not painted over the screen");

        restore(&terminal, &saved).expect("restoring works");
        assert_eq!(
            current_lflag(&terminal),
            before,
            "a terminal left in raw mode is one where the next shell shows no typing"
        );
    }

    #[test]
    fn output_processing_is_left_alone_because_this_terminal_is_shared() {
        // The textbook half of "raw mode" turns OPOST off, and it would be wrong here.
        // PID 1 writes its service lines to /dev/console, which is the foreground virtual
        // terminal, which is the one the dashboard draws on -- and without ONLCR those
        // lines staircase down the screen, each starting where the last ended. A drawing
        // that positions its own cursor does not care either way.
        //
        // Asserted rather than commented, because the next person to read `raw` will see a
        // function called "raw" that does not do the thing raw mode is famous for.
        let Some((terminal, _controller)) = a_terminal() else {
            println!("skip: no pty available on this host");
            return;
        };

        let before = current_oflag(&terminal);
        assert!(
            before & libc::OPOST != 0,
            "a fresh pty post-processes output"
        );
        let _saved = raw(&terminal).expect("a pty accepts termios");
        assert_eq!(
            current_oflag(&terminal) & libc::OPOST,
            libc::OPOST,
            "somebody else's newlines still have to work"
        );
    }

    fn current_oflag(terminal: &File) -> libc::tcflag_t {
        settings_of(terminal).c_oflag
    }

    fn current_lflag(terminal: &File) -> libc::tcflag_t {
        settings_of(terminal).c_lflag
    }

    fn settings_of(terminal: &File) -> libc::termios {
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: the descriptor is open and the local is the right type and outlives the
        // call; it is read only after tcgetattr reports success.
        let result = unsafe { libc::tcgetattr(terminal.as_raw_fd(), settings.as_mut_ptr()) };
        assert!(result >= 0, "reading a pty's settings must work");
        // SAFETY: tcgetattr succeeded, so the value is initialised.
        unsafe { settings.assume_init() }
    }
}
