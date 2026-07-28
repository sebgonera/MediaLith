//! Turning the appliance off from the console.
//!
//! The machine it runs on has no keyboard worth using and no shell a person is expected
//! to reach, so until now the only way to stop it was to hold the power button for five
//! seconds. That cuts power mid-write, and what is on `/var` — the media database, the
//! app image, the watch history — is exactly what a user would miss.
//!
//! # Ordering, and the one thing that is not obvious
//!
//! The sequence itself is unsurprising: stop Plex, `sync`, remount `/var` read-only,
//! `reboot(2)`. It lives in [`plexos_sys::power`], which is where the syscalls are
//! allowed to be.
//!
//! What is easy to get wrong is *when the browser is told*. `reboot(2)` does not return,
//! so a route that shut down before responding would close the socket from under the
//! request and the page would render a network error for a machine that did exactly what
//! it was asked. So the response goes out first and the work happens on a thread a moment
//! later. The delay is not a race being papered over: the socket is written and closed by
//! the time it elapses, and even if it were not, the failure mode would be a missing
//! confirmation rather than a machine in an unknown state.
//!
//! # Authentication
//!
//! `POST`, so [`crate::http::route`] demands the device token before this module is
//! reached. Turning off somebody's media server is exactly the kind of thing ADR-0013's
//! gate is for, and it needs no special case here.
//!
//! # What has run
//!
//! **No machine has been stopped by this.** Delete this notice when one has.

use std::sync::Arc;

use plexos_sys::power::{self, Action};
use plexos_types::paths;

/// How long the response is given to reach the browser before the machine goes.
///
/// One second. The socket is written and closed before this elapses; the delay exists so
/// that the *page* has visibly changed state, not because the write needs it.
pub const ANNOUNCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Reads the requested action out of a request body.
///
/// A tiny JSON document rather than two routes, because "off" and "restart" are the same
/// operation with a different last step, and a caller that can reach one can reach both.
///
/// Anything unrecognised is refused rather than defaulted. There is no safe default here:
/// guessing `restart` when somebody meant `off` leaves a machine running that was meant
/// to be silent, and guessing the other way takes down a server somebody wanted back.
#[must_use]
pub fn action_in(body: &[u8]) -> Option<Action> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    match parsed.get("action")?.as_str()? {
        "off" => Some(Action::Off),
        "restart" => Some(Action::Restart),
        _ => None,
    }
}

/// Performs the whole sequence. Does not return when it works.
///
/// Every step reports, and none of the recoverable ones abort the shutdown: a `/var` that
/// will not remount read-only because something holds a file open is a reason to say so,
/// not a reason to leave the user holding the power button after all — which is the
/// situation this replaces.
pub fn stop_now(action: Action, plex: &crate::plex::Handle, log: &mut dyn FnMut(&str)) {
    log(&format!("going to {} now", action.describe()));

    plex.stop(power::GRACE, log);

    power::sync();
    log("filesystem buffers flushed");

    match power::remount_read_only(std::path::Path::new(paths::VAR)) {
        Ok(()) => log(&format!(
            "{} is read-only; nothing is in flight",
            paths::VAR
        )),
        Err(error) => log(&format!(
            "could not remount {} read-only: {error}. Something still holds a file open \
             there. Going ahead anyway -- sync has run and XFS journals, so this is a \
             smaller risk than not stopping at all.",
            paths::VAR
        )),
    }

    // Twice, because the remount above may itself have written. Free, and the whole
    // point of the exercise.
    power::sync();

    match power::stop(action) {
        Ok(_) => unreachable!("reboot(2) does not return on success"),
        Err(error) => log(&format!(
            "the kernel refused to {}: {error}. This needs root, and plexosd is started \
             by PID 1 so it should have it -- if this is reachable, something is wrong \
             beyond the shutdown.",
            action.describe()
        )),
    }
}

/// Answers the request first, then stops the machine.
///
/// See the module documentation for why that order is not optional.
pub fn schedule(action: Action, plex: &Arc<crate::plex::Handle>) {
    let plex = Arc::clone(plex);
    std::thread::spawn(move || {
        std::thread::sleep(ANNOUNCE);
        stop_now(action, &plex, &mut |line| {
            println!("plexosd: power: {line}");
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_actions_are_read_from_the_body() {
        assert_eq!(action_in(br#"{"action":"off"}"#), Some(Action::Off));
        assert_eq!(action_in(br#"{"action":"restart"}"#), Some(Action::Restart));
    }

    #[test]
    fn anything_else_is_refused_rather_than_defaulted() {
        // There is no safe default. Guessing restart when somebody meant off leaves a
        // machine running that was meant to be silent; guessing the other way takes down
        // a server somebody wanted back.
        for body in [
            &b"{}"[..],
            br#"{"action":"halt"}"#,
            br#"{"action":""}"#,
            br#"{"action":42}"#,
            b"not json",
            b"",
        ] {
            assert_eq!(action_in(body), None, "{body:?} must not be guessed at");
        }
    }

    #[test]
    fn the_response_is_given_time_to_leave_before_the_machine_does() {
        // reboot(2) does not return, so shutting down before responding would close the
        // socket from under the request and the page would show a network error for a
        // machine that did exactly what it was told.
        assert!(ANNOUNCE.as_millis() >= 500);
        assert!(
            ANNOUNCE < power::GRACE,
            "the announcement must not be the slow part of a shutdown"
        );
    }
}
