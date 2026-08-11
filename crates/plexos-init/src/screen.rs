//! Letting the attached screen go dark.
//!
//! This appliance is a computer in a cupboard, and on the reference hardware it is a laptop
//! — so "the screen" is a panel with a backlight that was lit permanently, showing a text
//! console nobody is reading. The console is worth having: it is where the device token is
//! printed at first start, and where a boot that cannot reach the network says why. It is not
//! worth having lit all night.
//!
//! # This needs no program and no daemon
//!
//! The kernel's virtual terminal has a blank timer and a panel-powerdown timer of its own,
//! and both are set by writing an escape sequence to the terminal. `setterm` is the usual way
//! to send them and is not in this image — it is `util-linux`, and busybox has no equivalent —
//! but `setterm` only *writes bytes*, and PID 1 can write bytes. So this is two escape
//! sequences written once at boot: no package, no polling loop, and nothing to keep running.
//!
//! From `console_codes(4)`, read rather than recalled:
//!
//! ```text
//! ESC [ 9 ; n ]     Set screen blank timeout to n minutes.
//! ESC [ 14 ; n ]    Set the VESA powerdown interval in minutes.
//! ESC [ 13 ]        Unblank the screen.
//! ```
//!
//! **Both are needed, and the second is the one that matters here.** Blanking paints the
//! console black and leaves the backlight on, which on a laptop is a dark grey glow in a dark
//! room — most of the complaint, but not all of it. The powerdown interval is what turns the
//! panel off. Measured on the reference laptop rather than assumed: with the interval set to
//! one minute, `/sys/class/backlight/intel_backlight/bl_power` went from `0` to `4`, which is
//! `FB_BLANK_POWERDOWN`, and stayed there.
//!
//! # The timers are in whole minutes
//!
//! Which is why [`IDLE`] is expressed in minutes and not seconds. A duration that does not
//! divide would be silently truncated by the terminal, and a "90 second" setting that became
//! one minute is the kind of wrong that nobody notices.
//!
//! # `/dev/tty0`, never `/dev/console`
//!
//! `/dev/console` is whichever console the kernel command line named last, and this image's
//! command line is `… console=ttyS0,115200 console=tty0`. Today that resolves to the virtual
//! terminal, so writing to `/dev/console` would happen to work — and on a machine booted with
//! the order reversed, or with no `tty0` at all, the same write would send
//! `ESC[9;5]ESC[14;5]` down a serial line as literal rubbish to whatever is listening. The
//! virtual terminal is the thing being configured, so it is the thing to address.
//!
//! # What would make this look broken
//!
//! The blank timer is reset by any activity on the console — output included. So a service
//! that logged one cheerful line every thirty seconds would hold the screen awake for ever,
//! and the symptom would be "the screen never blanks" pointing at this file, which would be
//! innocent. It works today because `plex::supervise` is silent while things are healthy: its
//! happy path is a bare `continue`. That is worth knowing before adding a heartbeat log.
//!
//! # Why not `consoleblank=` on the command line
//!
//! `consoleblank=N` is the kernel parameter for the same blank timer, and it would need no
//! code at all. It sets only the blank half — there is no command-line parameter for the
//! powerdown interval — so it would leave the backlight on, which is the half of the problem
//! that is actually visible. Using both would put one behaviour in two places, and the one in
//! the command line is inside a signed UKI where changing it costs a rebuild. So: one place,
//! and it is this one. The kernel's own default is `consoleblank=0`, meaning never, which was
//! confirmed by reading `/sys/module/kernel/parameters/consoleblank` on the appliance — the
//! screen was not staying lit because of a setting, but because of the absence of one.
//!
//! # What has run
//!
//! **The sequences have run on the reference laptop**, written by hand through the console's
//! own terminal: at a one-minute interval the panel powered down as described above, and
//! setting five minutes and `ESC [ 13 ]` brought it back. This module is that write, moved
//! into the boot so it happens on every machine without anybody typing it.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

/// The virtual terminal whose timers these are. See the note above about `/dev/console`.
pub const CONSOLE: &str = "/dev/tty0";

/// How long the screen stays lit with nothing happening on it.
///
/// Five minutes, and the number is a compromise between two real uses rather than a default
/// copied from a desktop. Long enough that somebody reading a diagnostic off the panel — the
/// device token at first start, a boot that cannot reach the network — is not interrupted
/// while they find a pen. Short enough that an appliance left running in a room is not a lit
/// screen all night, which is what was asked for.
///
/// Whole minutes, because that is the unit the terminal takes.
pub const IDLE: Duration = Duration::from_secs(5 * 60);

/// The escape sequences that set both timers, for a duration in whole minutes.
///
/// Separated from the write so it can be tested: the bytes are the part that is easy to get
/// wrong and impossible to see afterwards, since a terminal given a malformed sequence
/// swallows it and reports nothing.
///
/// Zero disables both timers, which is what the terminal already does with `0` and is worth
/// being able to express — it is the way back for somebody who wants the screen to stay on.
#[must_use]
pub fn sequences(after: Duration) -> String {
    let minutes = after.as_secs() / 60;
    // Blank, then powerdown. Order does not matter to the terminal; this order matches the
    // way the two are described in console_codes(4), so the code reads like its own
    // documentation.
    format!("\x1b[9;{minutes}]\x1b[14;{minutes}]")
}

/// Asks the virtual terminal to blank and then power the panel down after `after`.
///
/// # Errors
/// Fails if the terminal cannot be opened or written. Both are expected on a machine with no
/// virtual terminal at all — a serial-only or headless boot has no `/dev/tty0` — which is why
/// the caller treats this as a comfort rather than a step that can fail a boot.
pub fn blank_after(console: &Path, after: Duration) -> std::io::Result<()> {
    let mut terminal = std::fs::OpenOptions::new().write(true).open(console)?;
    terminal.write_all(sequences(after).as_bytes())?;
    terminal.flush()
}

/// Applies [`IDLE`] to the console, reporting what happened and never failing a boot.
///
/// A screen that will not go dark is a comfort that did not arrive; a boot that stopped
/// because of one would be a far worse trade. The line is logged either way, because "the
/// screen never blanks" is otherwise unanswerable from outside the machine — and the log is
/// where somebody will look first.
pub fn arrange(log: &mut dyn FnMut(&str)) {
    let minutes = IDLE.as_secs() / 60;
    match blank_after(Path::new(CONSOLE), IDLE) {
        Ok(()) => log(&format!(
            "this screen will go dark after {minutes} minutes with nothing on it"
        )),
        Err(error) => log(&format!(
            "could not set the screen to blank ({CONSOLE}: {error}). Harmless: the console \
             stays lit. Remedy: on a machine with a virtual terminal, \
             printf '\\033[9;{minutes}]\\033[14;{minutes}]' > {CONSOLE}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sequences_are_the_ones_console_codes_documents() {
        // Pinned against the manual page rather than against this function: `ESC [ 9 ; n ]`
        // sets the blank timeout in minutes and `ESC [ 14 ; n ]` the VESA powerdown interval.
        // A test that only compared this to itself would pass just as happily with `8` and
        // `13`, which are "set palette" and "unblank" — the second of which would undo the
        // very thing being asked for.
        assert_eq!(sequences(IDLE), "\x1b[9;5]\x1b[14;5]");
        assert_eq!(sequences(Duration::from_secs(600)), "\x1b[9;10]\x1b[14;10]");
    }

    #[test]
    fn both_timers_are_set_because_blanking_alone_leaves_the_backlight_on() {
        let written = sequences(IDLE);
        assert!(
            written.contains("[9;"),
            "the blank timer paints the console black: {written:?}"
        );
        assert!(
            written.contains("[14;"),
            "and the powerdown timer is what turns the panel off, which is the half a laptop \
             owner actually sees: {written:?}"
        );
    }

    #[test]
    fn the_terminal_takes_whole_minutes_so_a_duration_is_reduced_to_them() {
        assert_eq!(sequences(Duration::from_secs(300)), "\x1b[9;5]\x1b[14;5]");
        // Ninety seconds is one minute to the terminal. Truncation is the terminal's own
        // behaviour; asserting it here is what stops somebody expressing a timeout that
        // silently becomes a different one.
        assert_eq!(sequences(Duration::from_secs(90)), "\x1b[9;1]\x1b[14;1]");
    }

    #[test]
    fn zero_is_expressible_because_it_is_how_the_screen_is_kept_on() {
        assert_eq!(sequences(Duration::ZERO), "\x1b[9;0]\x1b[14;0]");
    }

    #[test]
    fn the_default_is_five_minutes() {
        assert_eq!(IDLE, Duration::from_secs(300));
    }

    #[test]
    fn a_machine_with_no_virtual_terminal_is_not_a_failed_boot() {
        // Named for this test: Rust runs tests as threads in one process, so a fixed path is
        // one test deleting what another is reading.
        let absent = std::env::temp_dir().join("plexos-screen-no-such-terminal-test");
        let _ = std::fs::remove_file(&absent);

        assert!(blank_after(&absent, IDLE).is_err());

        // And the caller turns that into a line rather than into a stopped boot.
        let mut lines = Vec::new();
        arrange(&mut |line| lines.push(line.to_owned()));
        let logged = lines.join("\n");
        assert_eq!(lines.len(), 1, "exactly one line, either way: {logged}");
        assert!(
            logged.contains("5 minutes") || logged.contains("[9;5]"),
            "and it names the interval or the remedy: {logged}"
        );
    }
}
