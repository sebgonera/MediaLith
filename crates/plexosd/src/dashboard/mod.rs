//! The screen attached to the machine (ADR-0019).
//!
//! Until this existed, an appliance with a monitor plugged into it showed a scrolling log
//! and a root shell prompt — which is what a Linux server looks like, and this is not one.
//! Somebody who has just installed MediaLith and turned the monitor on should be able to
//! tell, without typing anything, whether the thing works and what address to point a
//! browser at.
//!
//! # Where this runs, and why that is the whole design
//!
//! **On a thread inside `plexosd`**, not as a process of its own. That decides more than it
//! looks like it does:
//!
//! - The pairing offer and the administrator sessions are in this process's memory, so
//!   there is no `/run` file, no fingerprint to keep in step, no clock to share across a
//!   process boundary, and no window in which two things can spend one code.
//! - Nothing on the network can start a pairing. The only caller of
//!   [`crate::pairing::start`] is the keyboard handler below.
//! - The recovery device code exists in a readable form for exactly as long as this thread
//!   holds it, which is from the moment `claim` issues one to the moment somebody presses a
//!   key. It is never written anywhere.
//!
//! # It owns `/dev/tty1`, and the log went to `/dev/tty2`
//!
//! The two cannot share. PID 1's log and everything `plexosd` prints used to go to
//! `/dev/console`, which is the foreground virtual terminal — so a dashboard drawn there
//! would be scribbled over by every line the daemon printed. `plexos-init` now gives the
//! console shell and `plexosd`'s own output `/dev/tty2`, which is one **Alt+F2** away and
//! is where somebody who wants a log or a shell has always been going anyway.
//!
//! PID 1's own service lines still go to `/dev/console`. That is deliberate: they are rare,
//! they are what a person needs if this dashboard is *not* on the screen, and a full
//! repaint a second later removes them.
//!
//! # It stops writing so the panel can go dark
//!
//! A frame identical to the one before it is not written at all, and after
//! [`QUIET_AFTER`] with nobody touching the keyboard nothing is written even if it changed.
//! Both are for one reason: the kernel's blank timer is reset by *any* output, so a screen
//! that repaints once a second is a laptop panel lit all night — which `plexos-init`'s
//! screen module exists to prevent and which this would silently have undone.
//!
//! # What has run
//!
//! **Nothing on hardware at the time of writing.** Everything except this file is exercised
//! by tests on a build host; this file is the part that can only be judged by looking at a
//! monitor.

pub mod model;
pub mod qr;
pub mod render;

use std::io::{Read as _, Write as _};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use model::{Facts, Transcoding};
use render::Screen;

/// The terminal the dashboard draws on.
///
/// `tty1` and not `/dev/console`: console is whichever terminal the kernel command line
/// named last and is shared with every log line on the machine. This is a specific screen,
/// owned by one thing.
pub const SCREEN: &str = "/dev/tty1";

/// How often the loop wakes.
///
/// Twice a second, which is fast enough that a keypress feels immediate and slow enough
/// that the countdown on the pairing screen never skips a second.
const TICK: Duration = Duration::from_millis(500);

/// How often the machine is asked about itself.
///
/// Three seconds. The health check opens a socket to Plex on loopback, so this is not free,
/// and nothing on this screen changes faster than a person can read it.
const FACTS_EVERY: Duration = Duration::from_secs(3);

/// How often the GPU report is regenerated.
///
/// Ten minutes, because generating it runs `vainfo` — and because what a graphics card can
/// do does not change between two frames of a dashboard. It changes across a reboot.
const GPU_EVERY: Duration = Duration::from_secs(600);

/// How long after the last keypress the screen stops being redrawn.
///
/// A minute. After this the dashboard writes nothing at all, so the kernel's own blank
/// timer runs out and the panel powers down — the behaviour `plexos_init::screen` was
/// written for, which a dashboard repainting on a timer would otherwise have made
/// impossible. Any keypress starts it again, and a keypress is also what unblanks the
/// screen, so the two agree without either knowing about the other.
const QUIET_AFTER: Duration = Duration::from_secs(60);

/// How long "Browser paired" and "Pairing code expired" stay up before the dashboard
/// returns.
const NOTICE: Duration = Duration::from_secs(8);

/// How long an unchanged screen goes before being painted again anyway.
///
/// Thirty seconds, and it exists because this dashboard does not own the terminal quite as
/// completely as it would like. PID 1's own service lines still go to `/dev/console`, which
/// is the foreground virtual terminal, which is this one — deliberately, because they are
/// what somebody needs if the dashboard is *not* on the screen. They are rare, and without
/// this they would sit in the middle of the drawing until something else happened to change
/// a frame, which on a healthy machine can be a minute.
///
/// It does not undo the silence: nothing is painted at all once [`QUIET_AFTER`] has passed,
/// so the kernel's blank timer still runs out and the panel still powers down. This only
/// bounds how long a stray line can sit on a screen somebody is standing at.
const REPAINT_EVERY: Duration = Duration::from_secs(30);

/// The size to draw at when the terminal will not say.
///
/// The smallest thing anybody has: at this size the pairing screen reports that it has no
/// room for a symbol rather than drawing a cropped one.
const FALLBACK: (usize, usize) = (24, 80);

/// Runs the dashboard for the life of the machine.
///
/// `first_boot_code` is the recovery device code in plaintext, and is `Some` only on the
/// boot that issued it. It is shown until somebody presses a key and then dropped — there
/// is no copy anywhere else, by design, so this is the one and only chance to read it.
///
/// Returns when the screen cannot be opened, which is an ordinary outcome on a headless
/// machine and must not be a fault: everything else about the appliance works without a
/// monitor, and always has.
pub fn run(first_boot_code: Option<String>, log: &mut dyn FnMut(&str)) {
    let screen = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(SCREEN)
    {
        Ok(file) => file,
        Err(error) => {
            log(&format!(
                "no dashboard on {SCREEN}: {error}. Harmless on a machine with no monitor \
                 — the console on the network is unaffected. Remedy, if there is a screen: \
                 check that the kernel has a virtual terminal (CONFIG_VT)."
            ));
            return;
        }
    };

    // Raw mode, so a key arrives without Enter and is not echoed over the drawing. A
    // failure here is not fatal: the dashboard still draws, and the keys simply need Enter
    // after them, which is a worse screen and not a broken appliance.
    match plexos_sys::tty::raw(&screen) {
        Ok(_) => {}
        Err(error) => log(&format!(
            "the screen's keyboard could not be put in raw mode: {error}. The dashboard \
             will draw, but a key may need Enter after it."
        )),
    }

    let size = plexos_sys::tty::size(&screen)
        .map_or(FALLBACK, |size| (size.rows as usize, size.columns as usize));
    log(&format!(
        "dashboard on {SCREEN}, {} rows by {} columns",
        size.0, size.1
    ));

    let keys = read_keys(&screen);
    draw(&screen, size, first_boot_code, &keys, log);
}

/// A thread that turns the keyboard into a channel.
///
/// A thread because the read blocks: `VMIN` is 1, so it waits for a key and costs nothing
/// while it does. The alternative is a poll with a timeout, which is a core spinning to
/// discover that nobody is standing at the machine.
fn read_keys(screen: &std::fs::File) -> mpsc::Receiver<u8> {
    let (sender, receiver) = mpsc::channel();
    if let Ok(mut reading) = screen.try_clone() {
        std::thread::spawn(move || {
            let mut byte = [0_u8; 1];
            while let Ok(1) = reading.read(&mut byte) {
                if sender.send(byte[0]).is_err() {
                    return;
                }
            }
        });
    }
    receiver
}

/// What the loop is showing and why, kept together so the transitions are one `match`.
struct State {
    screen: Screen,
    /// When the current notice should give way to the dashboard.
    notice_until: Option<Instant>,
    /// The recovery device code, while it is still on screen.
    first_boot_code: Option<String>,
    /// Whether the first-boot screen has already made its one unasked-for offer.
    first_boot_offered: bool,
}

/// The loop: read keys, re-read the machine, paint when it changed.
fn draw(
    screen: &std::fs::File,
    (rows, columns): (usize, usize),
    first_boot_code: Option<String>,
    keys: &mpsc::Receiver<u8>,
    log: &mut dyn FnMut(&str),
) {
    let mut writer = match screen.try_clone() {
        Ok(file) => file,
        Err(error) => {
            log(&format!("the screen cannot be written to: {error}"));
            return;
        }
    };

    let mut gpu = Transcoding::Unknown;
    let mut gpu_read: Option<Instant> = None;
    let mut facts = Facts::gather(&plexos_gpu::env::System, gpu);
    let mut facts_read = Instant::now();

    let mut state = State {
        screen: match &first_boot_code {
            // The one screen this appliance shows without being asked. A machine that has
            // just issued its recovery code has an owner standing in front of it, and the
            // code exists in a readable form only now.
            Some(_) => Screen::FirstBoot {
                url: None,
                recovery_code: String::new(),
            },
            None => Screen::Dashboard,
        },
        notice_until: None,
        first_boot_code,
        first_boot_offered: false,
    };

    let mut painted: Option<String> = None;
    let mut last_painted: Option<Instant> = None;
    let mut last_key = Instant::now();

    loop {
        let mut pressed = false;
        while let Ok(key) = keys.try_recv() {
            pressed = true;
            handle(key, &mut state, &facts, log);
        }
        if pressed {
            last_key = Instant::now();
            // A key that arrived while the panel was dark was spent waking it up as far as
            // the person is concerned, so the frame is forced rather than compared.
            painted = None;
        }

        if gpu_read.is_none_or(|read| read.elapsed() >= GPU_EVERY) {
            gpu = Transcoding::of(&plexos_gpu::report::Report::generate(
                &plexos_gpu::env::System,
            ));
            gpu_read = Some(Instant::now());
        }
        if facts_read.elapsed() >= FACTS_EVERY {
            facts = Facts::gather(&plexos_gpu::env::System, gpu);
            facts_read = Instant::now();
        }

        advance(&mut state, &facts);

        // Silence after a minute with nobody at the keyboard. Not a pause in the loop --
        // the state still advances, so whatever is on screen when somebody presses a key is
        // replaced immediately by the truth.
        if last_key.elapsed() < QUIET_AFTER {
            let next = render::frame(&state.screen, &facts, rows, columns);
            let overdue = last_painted.is_none_or(|at| at.elapsed() >= REPAINT_EVERY);
            if overdue || painted.as_deref() != Some(next.as_str()) {
                let _ = writer.write_all(next.as_bytes());
                let _ = writer.flush();
                painted = Some(next);
                last_painted = Some(Instant::now());
            }
        }

        std::thread::sleep(TICK);
    }
}

/// One keystroke.
///
/// The keys are the ones the screen offers and nothing else. In particular there is no key
/// that opens a shell: one already exists on the second virtual terminal, reached the way
/// it always has been, and adding a second door to it here would be widening an existing
/// decision rather than implementing this one.
fn handle(key: u8, state: &mut State, facts: &Facts, log: &mut dyn FnMut(&str)) {
    /// What the terminal sends for Escape.
    const ESC: u8 = 0x1b;

    // Any key at all dismisses the first-boot screen, and dismissing it is what drops the
    // recovery code out of memory. Deliberately any key rather than a named one: the
    // instruction on screen is "press any key when you have written it down", and a screen
    // that then ignored most of them would be the machine disagreeing with itself.
    if state.first_boot_code.is_some() {
        state.first_boot_code = None;
        crate::pairing::cancel();
        state.screen = Screen::Dashboard;
        log("the recovery device code has left the screen and this daemon's memory");
        return;
    }

    match key {
        b'p' | b'P' => {
            // Only where there is an address to put in it. A QR code pointing at nothing
            // is worse than no QR code: somebody scans it, gets an error from their
            // browser, and concludes the appliance is broken.
            if facts.address().is_some()
                && let Some(secret) = start_pairing(log)
            {
                state.screen = Screen::Pairing {
                    url: pairing_url(facts, &secret),
                    seconds_left: crate::pairing::LIFETIME.as_secs(),
                };
                state.notice_until = None;
            }
        }
        b'd' | b'D' => state.screen = Screen::Details,
        b'?' | b'/' | b'h' | b'H' => state.screen = Screen::Help,
        ESC | b'q' | b'Q' => {
            if matches!(state.screen, Screen::Pairing { .. }) {
                crate::pairing::cancel();
                log("pairing cancelled at the machine's own screen");
            }
            state.notice_until = None;
            state.screen = Screen::Dashboard;
        }
        _ => {}
    }
}

/// Puts a fresh code on offer, returning it so the caller can draw it.
fn start_pairing(log: &mut dyn FnMut(&str)) -> Option<String> {
    match crate::pairing::start() {
        // Nothing is logged about the code itself, here or anywhere. The line says that an
        // offer exists, which is what somebody reading a log needs, and the code is on a
        // monitor where it belongs.
        Ok(secret) => {
            log("a pairing code is on the attached screen for five minutes");
            return Some(secret);
        }
        Err(error) => log(&format!(
            "could not generate a pairing code: {error}. Remedy: use the recovery device \
             code in the browser. /dev/urandom being unreadable means /dev is not mounted, \
             which is a larger fault than this one."
        )),
    }
    None
}

/// Moves the screen on when something other than a keystroke changed.
///
/// Three transitions, and the interesting one is how "used" is told from "expired": the
/// offer is gone in both cases, and [`crate::pairing::Offers::is_expired`] is true only for
/// the second. So a browser that pairs while somebody is watching the screen sees the
/// screen say so, without the two ever talking to each other.
fn advance(state: &mut State, facts: &Facts) {
    if let Some(until) = state.notice_until
        && Instant::now() >= until
    {
        state.notice_until = None;
        state.screen = Screen::Dashboard;
    }

    match &state.screen {
        Screen::FirstBoot { .. } => {
            // The one offer nobody asked for, made when there is finally somewhere to point
            // it. A machine's first boot and its first DHCP lease are seconds apart and in
            // no fixed order — Ethernet arrives over USB and enumerates after PCI, which is
            // the fact ADR-0005's gate is built around — so an offer made only at start-up
            // would leave a new owner looking at "waiting for a network address" on a
            // machine that had an address by the time they read it.
            //
            // Once, and the `offered` flag is what makes it once. Renewing on every expiry
            // would leave a live credential standing on a screen nobody is in front of,
            // indefinitely; after this one runs out the screen says so and asks for P,
            // like every other pairing on this appliance.
            if !state.first_boot_offered
                && facts.address().is_some()
                && crate::pairing::secret().is_none()
            {
                state.first_boot_offered = true;
                let mut quiet = |_: &str| {};
                let _ = start_pairing(&mut quiet);
            }

            let code = state.first_boot_code.clone().unwrap_or_default();
            state.screen = Screen::FirstBoot {
                url: crate::pairing::secret().map(|secret| pairing_url(facts, &secret)),
                recovery_code: crate::auth::grouped(&crate::auth::normalise(&code)),
            };
        }
        Screen::Pairing { .. } => {
            if let Some(left) = crate::pairing::remaining() {
                if let Some(secret) = crate::pairing::secret() {
                    state.screen = Screen::Pairing {
                        url: pairing_url(facts, &secret),
                        seconds_left: left.as_secs(),
                    };
                }
            } else {
                state.screen = if crate::pairing::is_expired() {
                    Screen::PairingExpired
                } else {
                    Screen::Paired
                };
                state.notice_until = Some(Instant::now() + NOTICE);
            }
        }
        _ => {}
    }
}

/// What the QR code carries.
///
/// `https://<address>/#pair=<code>`, and every part of that is a decision:
///
/// - **The address** is the first of `reachable_at`, which is the same list the console
///   page tells somebody to type and the same one `/api/status` reports. A dashboard that
///   chose an interface for itself would eventually name a different one.
/// - **`https`** because that is the only thing this console serves; port 80 answers a
///   redirect and nothing else.
/// - **The fragment** because a fragment is not sent to the server. In a query string the
///   code would be in the request line, in the browser's history and in the address bar
///   over somebody's shoulder.
fn pairing_url(facts: &Facts, secret: &str) -> String {
    let address = facts.address().unwrap_or("");
    format!("https://{address}/#pair={secret}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Plex, Verdict};

    fn facts_at(address: Option<&str>) -> Facts {
        Facts {
            product: "MediaLith 0.1.0".to_owned(),
            version: Some("0.1.0".to_owned()),
            slot: Some("a".to_owned()),
            uptime: Some(Duration::from_secs(60)),
            addresses: address.map(|a| vec![a.to_owned()]).unwrap_or_default(),
            interface: address.map(|_| "eth0".to_owned()),
            plex: Plex::Running,
            transcoding: Transcoding::Ready,
            verdict: if address.is_some() {
                Verdict::Working
            } else {
                Verdict::NoNetwork
            },
        }
    }

    fn silent() -> impl FnMut(&str) {
        |_: &str| {}
    }

    #[test]
    fn the_pairing_url_puts_the_code_in_the_fragment_and_asks_for_tls() {
        // The whole payload, pinned. A query parameter would put the code in the request
        // line; http:// would point at a port that answers a redirect and nothing else.
        let url = pairing_url(&facts_at(Some("192.168.2.102")), "ABC123");
        assert_eq!(url, "https://192.168.2.102/#pair=ABC123");
        assert!(!url.contains('?'), "never a query parameter: {url}");
    }

    #[test]
    fn the_url_names_the_address_the_console_tells_people_to_type() {
        // Not an interface this module chose for itself. Two answers to "where is this
        // machine" is how a QR code comes to point at an address the browser cannot reach.
        let facts = Facts {
            addresses: vec!["192.168.2.102".to_owned(), "10.0.0.5".to_owned()],
            ..facts_at(Some("192.168.2.102"))
        };
        assert!(pairing_url(&facts, "X").starts_with("https://192.168.2.102/"));
    }

    #[test]
    fn pressing_p_with_no_address_does_nothing_at_all() {
        // A QR code pointing nowhere is worse than none: somebody scans it, their browser
        // reports an error, and they conclude the appliance is broken.
        let _serialised = crate::pairing::test_lock();
        let mut state = State {
            screen: Screen::Dashboard,
            notice_until: None,
            first_boot_code: None,
            first_boot_offered: true,
        };

        handle(b'P', &mut state, &facts_at(None), &mut silent());

        assert_eq!(state.screen, Screen::Dashboard);
        assert!(
            crate::pairing::secret().is_none(),
            "nothing was put on offer"
        );
    }

    #[test]
    fn pressing_p_offers_a_code_and_escape_takes_it_back() {
        let _serialised = crate::pairing::test_lock();
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = State {
            screen: Screen::Dashboard,
            notice_until: None,
            first_boot_code: None,
            first_boot_offered: true,
        };

        handle(b'p', &mut state, &facts, &mut silent());
        assert!(crate::pairing::secret().is_some(), "a code is on offer");

        advance(&mut state, &facts);
        let Screen::Pairing { url, .. } = &state.screen else {
            panic!("expected the pairing screen, got {:?}", state.screen);
        };
        assert!(url.contains("#pair="), "{url}");

        handle(0x1b, &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Dashboard);
        assert!(
            crate::pairing::secret().is_none(),
            "cancelling has to invalidate the code, not just stop drawing it"
        );
    }

    #[test]
    fn a_code_that_was_used_says_paired_and_one_that_ran_out_says_expired() {
        // The offer is gone in both cases and the screens must differ, because the remedies
        // do: one is finished and the other needs pressing P again. Nothing passes a
        // message between the browser and this loop -- `is_expired` distinguishes them.
        let _serialised = crate::pairing::test_lock();
        let facts = facts_at(Some("192.168.2.102"));
        let secret = crate::pairing::start().expect("/dev/urandom");
        let mut state = State {
            screen: Screen::Pairing {
                url: String::new(),
                seconds_left: 300,
            },
            notice_until: None,
            first_boot_code: None,
            first_boot_offered: true,
        };
        crate::pairing::consume(&secret).expect("the browser spends it");
        advance(&mut state, &facts);
        assert_eq!(state.screen, Screen::Paired);

        // And the other way: an offer whose deadline has passed. Driven through the struct
        // rather than the global, because waiting five minutes is not a test.
        let mut offers = crate::pairing::Offers::new();
        let t0 = Instant::now();
        offers.offer("TIMED".to_owned(), t0);
        assert!(offers.is_expired(t0 + crate::pairing::LIFETIME));
        assert!(offers.remaining(t0 + crate::pairing::LIFETIME).is_none());
    }

    #[test]
    fn any_key_dismisses_the_first_boot_screen_and_drops_the_code() {
        // The instruction on screen is "press any key when you have written it down". A
        // screen that then ignored most of them would be the machine disagreeing with
        // itself -- and the code has to leave memory when it leaves the screen, because
        // that is the only guarantee this feature makes about it.
        let _serialised = crate::pairing::test_lock();
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = State {
            screen: Screen::FirstBoot {
                url: None,
                recovery_code: String::new(),
            },
            notice_until: None,
            first_boot_code: Some("4K7QM2XR9T8BHVWP".to_owned()),
            first_boot_offered: false,
        };

        handle(b'x', &mut state, &facts, &mut silent());

        assert!(state.first_boot_code.is_none(), "the plaintext is gone");
        assert_eq!(state.screen, Screen::Dashboard);
        assert!(
            crate::pairing::secret().is_none(),
            "and the code offered alongside it goes too, rather than standing on a screen \
             nobody is looking at"
        );
    }

    #[test]
    fn first_boot_offers_its_qr_when_the_address_arrives_and_only_once() {
        // A machine's first boot and its first DHCP lease are seconds apart in no fixed
        // order: Ethernet arrives over USB and enumerates after PCI, which is the fact
        // ADR-0005's gate is built around. An offer made only at start-up would leave a new
        // owner reading "waiting for a network address" on a machine that had one.
        let _serialised = crate::pairing::test_lock();
        let mut state = State {
            screen: Screen::FirstBoot {
                url: None,
                recovery_code: String::new(),
            },
            notice_until: None,
            first_boot_code: Some("4K7QM2XR9T8BHVWP".to_owned()),
            first_boot_offered: false,
        };

        // No cable yet: nothing is offered, and the screen says what it is waiting for.
        advance(&mut state, &facts_at(None));
        assert!(crate::pairing::secret().is_none());
        let Screen::FirstBoot { url, .. } = &state.screen else {
            panic!("expected the first-boot screen");
        };
        assert!(url.is_none());

        // The lease arrives.
        advance(&mut state, &facts_at(Some("192.168.2.102")));
        let Screen::FirstBoot { url, .. } = &state.screen else {
            panic!("expected the first-boot screen");
        };
        assert!(
            url.as_deref().is_some_and(|url| url.contains("#pair=")),
            "the QR appears without anybody pressing anything: {url:?}"
        );

        // And exactly once. Renewing on every expiry would leave a live credential standing
        // on a screen nobody is in front of, for as long as the machine is on.
        crate::pairing::cancel();
        advance(&mut state, &facts_at(Some("192.168.2.102")));
        assert!(
            crate::pairing::secret().is_none(),
            "after the first one runs out the screen asks for P, like every other pairing"
        );
    }

    #[test]
    fn the_first_boot_screen_shows_the_code_in_the_shape_it_was_printed_in() {
        // Four groups of four. The same function the log banner uses, so the screen and the
        // banner cannot disagree about what somebody is supposed to type.
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = State {
            screen: Screen::FirstBoot {
                url: None,
                recovery_code: String::new(),
            },
            notice_until: None,
            first_boot_code: Some("4K7QM2XR9T8BHVWP".to_owned()),
            first_boot_offered: false,
        };

        advance(&mut state, &facts);
        let Screen::FirstBoot { recovery_code, .. } = &state.screen else {
            panic!("expected the first-boot screen");
        };
        assert_eq!(recovery_code, "4K7Q-M2XR-9T8B-HVWP");
    }

    #[test]
    fn details_and_help_are_reachable_and_escape_comes_back() {
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = State {
            screen: Screen::Dashboard,
            notice_until: None,
            first_boot_code: None,
            first_boot_offered: true,
        };

        handle(b'd', &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Details);
        handle(0x1b, &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Dashboard);

        handle(b'?', &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Help);
        handle(0x1b, &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Dashboard);
    }

    #[test]
    fn no_key_opens_a_shell() {
        // One already exists on the second virtual terminal, reached the way it always has
        // been. Adding a door to it here would be widening an existing decision under
        // cover of implementing a different one -- and an unauthenticated root shell is not
        // something to acquire a second entrance to by accident.
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = State {
            screen: Screen::Dashboard,
            notice_until: None,
            first_boot_code: None,
            first_boot_offered: true,
        };

        for key in b"sSfF0123456789\t\r\n" {
            handle(*key, &mut state, &facts, &mut silent());
            assert_eq!(
                state.screen,
                Screen::Dashboard,
                "{:?} moved the screen somewhere",
                *key as char
            );
        }
    }
}
