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
pub fn run(
    first_boot_code: Option<String>,
    plex: &std::sync::Arc<crate::plex::Handle>,
    wifi: &std::sync::Arc<crate::wifi::Job>,
    log: &mut dyn FnMut(&str),
) {
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

    // Before the size is read, because changing the glyph size changes the grid: 8x16 on
    // this panel is 360x101 and TER16x32 is 180x50, and laying out for the wrong one puts
    // the footer off the bottom of the screen.
    choose_a_legible_font(&screen, log);

    let size = plexos_sys::tty::size(&screen)
        .map_or(FALLBACK, |size| (size.rows as usize, size.columns as usize));
    log(&format!(
        "dashboard on {SCREEN}, {} rows by {} columns",
        size.0, size.1
    ));

    // After the size is reported and before anything is drawn: from here the screen is a
    // dashboard rather than a log, and the kernel is the one writer left that does not
    // know that.
    quieten_the_kernel(log);

    let keys = read_keys(&screen);
    draw(&screen, size, first_boot_code, plex, wifi, &keys, log);
}

/// What a keystroke means, once the escape sequences have been put back together.
///
/// The terminal sends an arrow as three bytes -- `ESC [ A` -- and this screen read them one
/// at a time, so the first byte hit the branch that cancels. Pressing an arrow key cancelled
/// an active pairing offer; so did Home, End, and every function key, because all of them
/// begin with `ESC`. Nothing on the screen said why the code had gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// An ordinary printable byte.
    Char(u8),
    /// Escape on its own, which is the only one that cancels.
    Escape,
    /// Arrow up, for anything on this screen that is a list.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Home, which arrives the same way and would otherwise cancel too.
    Home,
    /// End, likewise.
    End,
}

/// Turns the bytes that have arrived into keys, leaving an unfinished sequence behind.
///
/// `settled` is the whole of how a lone Escape is told from the start of an arrow: they are
/// the same first byte and only time separates them. The draw loop passes `true` when a tick
/// went by with nothing more arriving, so a bare Escape costs one tick and a sequence costs
/// nothing -- its remaining bytes are already in the buffer by the time it is asked.
///
/// Pure, and separate from the channel it is fed from, so every one of these can be a test
/// rather than somebody standing at a machine pressing keys.
pub fn keys_from(buffer: &mut Vec<u8>, settled: bool) -> Vec<Key> {
    let mut out = Vec::new();
    loop {
        let Some(&first) = buffer.first() else {
            return out;
        };
        if first != 0x1b {
            out.push(Key::Char(first));
            buffer.remove(0);
            continue;
        }
        // An escape, and what follows decides what it was.
        match buffer.get(1) {
            // Nothing yet. If the tick settled, nothing more is coming and this was the key
            // itself; otherwise leave it and look again next time.
            None => {
                if settled {
                    buffer.remove(0);
                    out.push(Key::Escape);
                    continue;
                }
                return out;
            }
            // CSI. `ESC O` is the other introducer -- some terminals send arrows in
            // application mode -- and it carries the same final bytes.
            Some(b'[' | b'O') => match buffer.get(2) {
                None => {
                    if settled {
                        // A two-byte sequence that stopped. Nothing to act on, and holding
                        // it would make the next keystroke part of a sequence that ended.
                        buffer.drain(..2);
                        continue;
                    }
                    return out;
                }
                Some(&final_byte) => {
                    let key = match final_byte {
                        b'A' => Some(Key::Up),
                        b'B' => Some(Key::Down),
                        b'C' => Some(Key::Right),
                        b'D' => Some(Key::Left),
                        b'H' => Some(Key::Home),
                        b'F' => Some(Key::End),
                        _ => None,
                    };
                    // `ESC [ 1 ~` and friends: a numeric parameter runs to a letter or `~`.
                    // Consumed and ignored rather than guessed at -- an unrecognised
                    // sequence must not fall through to the byte after it, which is how a
                    // function key would have cancelled.
                    if key.is_none() && final_byte.is_ascii_digit() {
                        let end = buffer
                            .iter()
                            .skip(2)
                            .position(|b| *b == b'~' || b.is_ascii_alphabetic());
                        match end {
                            Some(offset) => {
                                buffer.drain(..3 + offset);
                                continue;
                            }
                            None => return out,
                        }
                    }
                    buffer.drain(..3);
                    if let Some(key) = key {
                        out.push(key);
                    }
                }
            },
            // `ESC` followed by something that is not an introducer: Alt+key on most
            // terminals. Neither byte is acted on, and neither is left to be read as a key
            // of its own.
            Some(_) => buffer.drain(..2).for_each(drop),
        }
    }
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
    /// Set when somebody has confirmed a restart or a shutdown, and cleared when it starts.
    ///
    /// The keyboard handler records the decision and **does not act on it**. Two reasons,
    /// and the second is the one that matters. The screen is painted before the machine
    /// begins to stop, so somebody who pressed the key sees that it was taken rather than
    /// watching a menu for the several seconds it takes to stop Plex and flush a disk. And
    /// [`handle`] stays a pure function of a keystroke and a state, which is what lets every
    /// path through it be a test — a handler that called `power::schedule` would be one no
    /// test could exercise without restarting the machine running the suite.
    going: Option<plexos_sys::power::Action>,
    /// What the keyboard asked the radio to do, performed by the draw loop.
    ///
    /// The same arrangement as [`State::going`] and for the same two reasons: `handle` stays
    /// a pure function, and the frame showing what is about to happen is painted before the
    /// thing happens.
    radio: Option<Radio>,
    /// The passphrase being typed, while it is being typed.
    ///
    /// **Here and nowhere else.** The render model carries only how many characters have
    /// been entered, so there is nothing in the thing that paints the screen that *could* be
    /// drawn in clear even by a mistake. It is cleared on every way off that screen.
    typed: String,
}

/// What the keyboard asked the radio to do.
enum Radio {
    /// Look for what is in range.
    Scan,
    /// Join one.
    Join {
        /// The network's name.
        ssid: String,
        /// The passphrase, empty for an open network.
        passphrase: String,
        /// What the scan said the network expects, so the right kind of credential is kept.
        security: crate::wifi::Security,
    },
}

/// The loop: read keys, re-read the machine, paint when it changed.
fn draw(
    screen: &std::fs::File,
    (rows, columns): (usize, usize),
    first_boot_code: Option<String>,
    plex: &std::sync::Arc<crate::plex::Handle>,
    wifi: &std::sync::Arc<crate::wifi::Job>,
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

    // The report is kept, not just the verdict it produced: `Status` wants one, and
    // generating a second would put `vainfo` back on the three-second path this cache
    // exists to keep it off.
    let mut report = plexos_gpu::report::Report::generate(&plexos_gpu::env::System);
    let mut gpu = Transcoding::of(&report);
    let mut gpu_read = Instant::now();
    let mut facts = Facts::gather(&plexos_gpu::env::System, report.clone(), gpu);
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
        going: None,
        radio: None,
        typed: String::new(),
    };

    // Bytes that have arrived and are not yet a whole key. Never more than a few, because
    // an escape sequence is three bytes and anything longer is consumed as one.
    let mut pending: Vec<u8> = Vec::new();
    let mut painted: Option<String> = None;
    let mut last_painted: Option<Instant> = None;
    let mut last_key = Instant::now();

    loop {
        // Bytes in, keys out. `settled` is true when a whole tick went by with nothing
        // more arriving, which is what lets a lone Escape resolve without swallowing the
        // start of an arrow that is still on its way.
        let mut arrived = false;
        while let Ok(byte) = keys.try_recv() {
            arrived = true;
            pending.push(byte);
        }
        let struck = keys_from(&mut pending, !arrived);
        let pressed = !struck.is_empty();
        for key in struck {
            handle(key, &mut state, &facts, log);
        }
        if pressed {
            last_key = Instant::now();
            // A key that arrived while the panel was dark was spent waking it up as far as
            // the person is concerned, so the frame is forced rather than compared.
            painted = None;
        }

        if gpu_read.elapsed() >= GPU_EVERY {
            report = plexos_gpu::report::Report::generate(&plexos_gpu::env::System);
            gpu = Transcoding::of(&report);
            gpu_read = Instant::now();
        }
        if facts_read.elapsed() >= FACTS_EVERY {
            facts = Facts::gather(&plexos_gpu::env::System, report.clone(), gpu);
            facts_read = Instant::now();
        }

        // Read once a tick and passed in, so `advance` is a function of a value rather than
        // of a mutex — which is what lets every wireless transition below be a test.
        let radio = wifi.snapshot();
        advance(&mut state, &facts, &radio);

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

        // After the paint and not before it: the frame saying "Restarting..." has to be on
        // the screen before the machine begins to stop, because stopping Plex and flushing
        // the disk takes seconds and a screen still showing a menu through all of it reads
        // as a key that was ignored by a machine that then died.
        //
        // `take` so it happens once. `schedule` does not return in the sense that matters --
        // the thread it spawns ends in `reboot(2)` -- but a loop that asked twice would put
        // two shutdown sequences on the same Plex.
        if let Some(action) = state.going.take() {
            crate::power::schedule(action, plex);
        }

        // The same claim the console's own route makes, so the screen and the page cannot
        // both hold the radio: two supplicants on one interface is a machine that associates
        // and immediately disassociates, repeatedly, for no visible reason. Losing the claim
        // is not an error worth a screen -- the job the loser would have started is already
        // running and its progress is what both of them are showing.
        match state.radio.take() {
            Some(Radio::Scan) => {
                if wifi.begin(crate::wifi::Phase::Scanning, "scanning for networks") {
                    crate::wifi::spawn_scan(wifi);
                }
            }
            Some(Radio::Join {
                ssid,
                passphrase,
                security,
            }) => {
                if wifi.begin(crate::wifi::Phase::Associating, &format!("joining {ssid}")) {
                    // `hidden` is false because every row on that list came from a scan, and
                    // a scan reports the networks that broadcast a name. Typing the name of
                    // one that does not is a keyboard this screen has not been given.
                    crate::wifi::spawn_join(wifi, ssid, passphrase, false, Some(security));
                }
            }
            None => {}
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
fn handle(key: Key, state: &mut State, facts: &Facts, log: &mut dyn FnMut(&str)) {
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

    // The screen decides before the key does, because some screens spend keys the dashboard
    // has other meanings for. `Y` is a letter on the dashboard and an answer on a
    // confirmation, and a handler that read the key first would have to know about every
    // screen in the same `match`.
    match state.screen {
        Screen::Power { choice } => return power_key(key, choice, state),
        Screen::PowerConfirm { choice } => return confirm_key(key, choice, state, log),
        // Nothing is offered, because there is nothing left to offer: the machine is on its
        // way down and a key that appeared to cancel it would be lying.
        Screen::PowerGoing { .. } => return,
        // Every printable byte on this screen is a character of a passphrase, including the
        // ones the dashboard has other meanings for. A handler that read the key first would
        // have typed `p` and put a pairing code on the screen instead.
        Screen::WirelessKey { .. } => return typing(key, state),
        Screen::Wireless { .. } => return network_key(key, state, log),
        Screen::WirelessJoining { .. } => {
            if matches!(key, Key::Escape | Key::Char(b'q' | b'Q')) {
                state.screen = empty_list();
            }
            return;
        }
        // A notice, so anything dismisses it.
        Screen::WirelessJoined { .. } => {
            state.notice_until = None;
            state.screen = Screen::Dashboard;
            return;
        }
        _ => {}
    }

    match key {
        Key::Char(b'p' | b'P') => {
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
        Key::Char(b'd' | b'D') => state.screen = Screen::Details,
        Key::Char(b'o' | b'O') => {
            // Restart first. It is the one somebody standing at a misbehaving appliance
            // wants, and it is the one that leaves the machine reachable afterwards -- so
            // the row under the cursor when the screen opens is the recoverable one.
            state.screen = Screen::Power {
                choice: plexos_sys::power::Action::Restart,
            };
        }
        Key::Char(b'w' | b'W') => {
            // Only where there is a radio. A list that can never have anything in it is the
            // same mistake as a QR code pointing nowhere: somebody presses the key, sees
            // nothing, and concludes the appliance is broken.
            if facts.wireless.is_some() {
                state.screen = empty_list();
                // Immediately, without asking. Somebody who pressed W wants to see what is
                // in range, and a list that opened empty with a "scan" key on it would be
                // asking them to confirm the thing they just asked for.
                state.radio = Some(Radio::Scan);
            }
        }
        Key::Char(b'?' | b'/' | b'h' | b'H') => state.screen = Screen::Help,
        // Escape on its own, and never the first byte of an arrow: pressing Up used to
        // cancel a pairing offer, because the three bytes of the arrow arrived one at a
        // time and the first of them is this.
        Key::Escape | Key::Char(b'q' | b'Q') => {
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

/// A wireless list with nothing in it yet.
///
/// The rows are filled in by [`advance`] from the radio's own job, so this is the state the
/// screen is in for the fraction of a second before the first scan reports — and the state
/// it goes back to from every way off the screens below it.
fn empty_list() -> Screen {
    Screen::Wireless {
        rows: Vec::new(),
        choice: 0,
        scanning: false,
        note: None,
    }
}

/// The wireless list: move the cursor, take a row, scan again, or leave.
fn network_key(key: Key, state: &mut State, log: &mut dyn FnMut(&str)) {
    match key {
        // Clamped rather than wrapped, unlike the two-row power menu. A list of a dozen
        // networks that jumps from the last back to the first is one where somebody holding
        // Down to reach the bottom sails past it without noticing.
        Key::Up | Key::Down | Key::Home | Key::End => {
            if let Screen::Wireless { rows, choice, .. } = &mut state.screen {
                let last = rows.len().saturating_sub(1);
                *choice = match key {
                    Key::Up => choice.saturating_sub(1),
                    Key::Down => (*choice + 1).min(last),
                    Key::Home => 0,
                    _ => last,
                };
            }
        }
        Key::Char(b'r' | b'R') => {
            if let Screen::Wireless { note, .. } = &mut state.screen {
                *note = None;
            }
            state.radio = Some(Radio::Scan);
        }
        Key::Char(b'\r' | b'\n') => {
            let Screen::Wireless { rows, choice, .. } = &state.screen else {
                return;
            };
            let Some((ssid, security)) = rows
                .get(*choice)
                .map(|row| (row.ssid.clone(), row.security))
            else {
                return;
            };
            // The remedy in full, at the moment somebody asked for the thing it refuses.
            // `Security::refusal` writes both of these and each names what to change on the
            // access point, which is the only place either can be fixed.
            if let Some(refusal) = security.refusal() {
                log(&format!("{ssid} cannot be joined: {refusal}"));
                if let Screen::Wireless { note, .. } = &mut state.screen {
                    *note = Some(refusal.to_owned());
                }
                return;
            }
            state.typed.clear();
            state.screen = Screen::WirelessKey {
                ssid,
                typed: 0,
                open: security == crate::wifi::Security::Open,
            };
        }
        Key::Escape | Key::Char(b'q' | b'Q') => state.screen = Screen::Dashboard,
        _ => {}
    }
}

/// Typing a passphrase.
///
/// Every printable byte is a character rather than a command, which is why this screen is
/// dispatched to before the keys are read at all: `p` here is a letter of a passphrase, and
/// on the dashboard it puts a pairing code on the screen.
fn typing(key: Key, state: &mut State) {
    /// Backspace, as a terminal in raw mode actually sends it.
    ///
    /// Both, because which one arrives depends on the terminal rather than on the keyboard —
    /// the Linux console sends `DEL` and a great many others send `BS`. Accepting one is a
    /// screen where the correction key does nothing on somebody else's machine.
    const ERASE: [u8; 2] = [0x7f, 0x08];

    let Screen::WirelessKey { ssid, open, .. } = &state.screen else {
        return;
    };
    let (ssid, open) = (ssid.clone(), *open);

    match key {
        Key::Char(b'\r' | b'\n') => {
            // An empty passphrase on a network that wants one is refused here rather than
            // sent: the supplicant's answer would be twenty-five seconds of retrying
            // followed by a timeout, which reads as a wrong passphrase rather than as none.
            if !open && state.typed.is_empty() {
                return;
            }
            state.radio = Some(Radio::Join {
                ssid: ssid.clone(),
                // Moved out rather than copied, so after this line the only place it exists
                // is the message on its way to the supplicant.
                passphrase: std::mem::take(&mut state.typed),
                security: if open {
                    crate::wifi::Security::Open
                } else {
                    crate::wifi::Security::Psk
                },
            });
            state.screen = Screen::WirelessJoining {
                ssid,
                detail: "starting".to_owned(),
                error: None,
            };
            return;
        }
        Key::Escape => {
            // Cleared on the way out, which is the whole reason this is a branch of its own
            // rather than something that falls through: a passphrase left in memory because
            // somebody changed their mind is a passphrase nobody knows is there.
            state.typed.clear();
            state.screen = empty_list();
            return;
        }
        Key::Char(byte) if ERASE.contains(&byte) => {
            state.typed.pop();
        }
        // Printable ASCII and nothing else. A control character in a passphrase is a
        // keystroke that was meant as a command, and a passphrase field is not somewhere to
        // find out that the machine took it literally.
        Key::Char(byte) if byte.is_ascii_graphic() || byte == b' ' => {
            state.typed.push(byte as char);
        }
        _ => return,
    }

    // One place where what is drawn is derived from what is held, so the two cannot drift.
    if let Screen::WirelessKey { typed, .. } = &mut state.screen {
        *typed = state.typed.chars().count();
    }
}

/// The power menu: move the cursor, or take the row it is on.
fn power_key(key: Key, choice: plexos_sys::power::Action, state: &mut State) {
    use plexos_sys::power::Action;

    match key {
        // Two rows, so either arrow is the other one. Wrapping rather than stopping at the
        // ends: a list of two where Up does nothing on the first row is a list that appears
        // not to respond to half the presses.
        Key::Up | Key::Down => {
            state.screen = Screen::Power {
                choice: match choice {
                    Action::Restart => Action::Off,
                    Action::Off => Action::Restart,
                },
            };
        }
        Key::Char(b'\r' | b'\n') => state.screen = Screen::PowerConfirm { choice },
        Key::Escape | Key::Char(b'q' | b'Q') => state.screen = Screen::Dashboard,
        _ => {}
    }
}

/// The confirmation: one named key, and everything else goes back.
///
/// Deliberately not Enter. Enter is what opened this screen, so accepting it here would make
/// two presses of one key into a shutdown — on a screen that sits in a room where somebody
/// leaning on a desk is a keystroke.
fn confirm_key(
    key: Key,
    choice: plexos_sys::power::Action,
    state: &mut State,
    log: &mut dyn FnMut(&str),
) {
    match key {
        Key::Char(b'y' | b'Y') => {
            // Recorded, not done. The draw loop paints this frame and then starts the
            // sequence, so the screen says what is happening before the machine begins to
            // stop -- and this function stays testable, which a call to `power::schedule`
            // would not be.
            state.going = Some(choice);
            state.screen = Screen::PowerGoing { choice };
            log(&format!(
                "{} asked for at the machine's own screen",
                choice.describe()
            ));
        }
        // Back to the menu rather than to the dashboard. Somebody who answered "no" to
        // shutting down may well have meant restart, and making them press O again to find
        // out is the machine being pedantic about a keystroke.
        Key::Escape | Key::Char(b'n' | b'N' | b'q' | b'Q') => {
            state.screen = Screen::Power { choice };
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
fn advance(state: &mut State, facts: &Facts, radio: &crate::wifi::Progress) {
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
        // The list is rebuilt from the radio's own job on every tick rather than kept, so a
        // scan that finishes while somebody is looking at the screen fills it in without a
        // keystroke -- and there is no second copy of what is in range to go stale.
        Screen::Wireless { choice, note, .. } => {
            let (choice, note) = (*choice, note.clone());
            let saved = crate::wifi::saved().map(|network| network.ssid);
            let rows: Vec<_> = radio
                .networks
                .iter()
                .map(|network| render::WirelessRow {
                    ssid: network.ssid.clone(),
                    bars: bars_for(network.strength()),
                    five_ghz: network.is_5ghz(),
                    security: network.security,
                    saved: saved.as_deref() == Some(network.ssid.as_str()),
                })
                .collect();
            state.screen = Screen::Wireless {
                // Clamped rather than reset. A scan that comes back with one network fewer
                // must not move somebody's cursor to the top of the list they were reading.
                choice: choice.min(rows.len().saturating_sub(1)),
                rows,
                scanning: radio.phase == crate::wifi::Phase::Scanning,
                // A scan's own failure is worth showing here; a note somebody's keystroke
                // produced takes precedence, because it is the more recent answer to the
                // more specific question.
                note: note.or_else(|| {
                    (radio.phase == crate::wifi::Phase::Failed)
                        .then(|| radio.error.clone())
                        .flatten()
                }),
            };
        }
        Screen::WirelessJoining { ssid, .. } => {
            let ssid = ssid.clone();
            match radio.phase {
                crate::wifi::Phase::Connected => {
                    state.screen = Screen::WirelessJoined { ssid };
                    state.notice_until = Some(Instant::now() + NOTICE);
                }
                _ => {
                    state.screen = Screen::WirelessJoining {
                        ssid,
                        detail: radio.detail.clone(),
                        error: radio.error.clone(),
                    };
                }
            }
        }
        _ => {}
    }

    // Nothing wireless survives the radio disappearing. A machine whose adapter was
    // unplugged while somebody was reading the list would otherwise sit on a list of
    // networks it can no longer see, with a join key that could never do anything.
    if facts.wireless.is_none()
        && matches!(
            state.screen,
            Screen::Wireless { .. }
                | Screen::WirelessKey { .. }
                | Screen::WirelessJoining { .. }
                | Screen::WirelessJoined { .. }
        )
    {
        state.typed.clear();
        state.screen = Screen::Dashboard;
    }
}

/// A signal, as whole bars.
///
/// `strength` is already the linear share of usable range that the console page draws, so
/// this is the same number rounded to the resolution a console has. Deriving it here rather
/// than taking dBm means the screen and the page cannot disagree about how strong a network
/// is, which is the sort of thing somebody notices when they have both open.
fn bars_for(strength: f32) -> u8 {
    // Counted rather than cast. Turning a float into an integer here would be two lint
    // exceptions and a truncation nobody checks, for a number with four possible values.
    //
    // It starts at one, which is the deliberate part: zero bars beside a network the scan
    // has just found reads as a fault rather than as a weak signal, and the scan finding it
    // means something reached this machine.
    let scaled = strength * f32::from(render::BARS);
    let mut filled = 1;
    for bar in 2..=render::BARS {
        if scaled > f32::from(bar) - 1.0 {
            filled = bar;
        }
    }
    filled
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
    // `facts.address()` and not a choice made here. Which address to put in front of
    // somebody is decided once, in `Facts::gather`, and the screen prints the same one it
    // encodes -- a QR and a printed URL that disagree is a machine contradicting itself.
    format!("https://{}/#pair={secret}", facts.address().unwrap_or(""))
}

/// Asks for the largest built-in font **that still leaves room for the screen**.
///
/// Largest is not the answer, and a machine said so within an hour of the last one being
/// shipped. The reference laptop is 2880x1620, where `TER16x32` gives 50 rows by 180 and
/// text somebody can read across a room. A 1920x1080 panel with the same font is **33
/// rows**, and the pairing screen needs 47 — so the QR code did not appear at all, on the
/// machine whose owner had just asked for bigger text.
///
/// So it tries largest first and *measures the result*: a font is accepted only if the grid
/// it produces can still hold everything this dashboard draws. The fallback is not a
/// smaller screen, it is a smaller font, and on a 1080p panel that is `TER10x18` — 60 rows
/// by 192, still twice the area of the kernel's default.
///
/// The measurement is the point. Choosing by resolution would mean a table of panels, and
/// the numbers that matter are the ones the terminal reports after the change rather than
/// the ones arithmetic predicts from a framebuffer.
fn choose_a_legible_font(screen: &std::fs::File, log: &mut dyn FnMut(&str)) {
    /// Largest first. `TER16x32` and `TER10x18` need `CONFIG_FONTS`; `VGA8x16` is what the
    /// kernel falls back to on its own and is always there.
    const PREFERRED: &[&str] = &["TER16x32", "TER10x18", "VGA8x16"];

    let mut rejected = Vec::new();
    for name in PREFERRED {
        if let Err(error) = plexos_sys::tty::use_font(screen, name) {
            rejected.push(format!("{name}: {error}"));
            continue;
        }

        // What the terminal says it is now, not what the font's name implies. A font that
        // was accepted and produced a grid too small for the pairing screen is worse than
        // the one before it -- the text is bigger and the thing somebody came to the screen
        // for is missing.
        let Ok(size) = plexos_sys::tty::size(screen) else {
            // No measurement, so no basis to reject it. The dashboard falls back to 24x80
            // and reports that it has no room, which is at least true.
            return;
        };
        if fits(size) {
            if !rejected.is_empty() {
                log(&format!(
                    "the screen is using {name} at {} by {}; larger fonts left too little \
                     room ({})",
                    size.rows,
                    size.columns,
                    rejected.join(", ")
                ));
            }
            return;
        }
        rejected.push(format!(
            "{name}: {} rows by {} is too few for a pairing code",
            size.rows, size.columns
        ));
    }

    log(&format!(
        "no console font left room for this dashboard ({}). The screen still works. \
         Remedy: check that CONFIG_FONTS and CONFIG_FONT_TER10x18 both survived kconfig -- \
         the second depends on the first and is dropped without an error when it is \
         missing.",
        rejected.join("; ")
    ));
}

/// Whether a grid can hold everything this dashboard draws.
///
/// The binding case is the pairing screen: a symbol of [`qr::Symbol::drawn_width`] rows plus
/// the wordmark, the countdown and the footer around it. The dashboard itself needs about
/// twenty rows and fits anywhere.
///
/// Deliberately a little more than the exact arithmetic. A grid that fits by one row is one
/// where the next line of text added to that screen breaks the QR code on somebody else's
/// monitor, and the cost of asking for slack is a font one size down.
fn fits(size: plexos_sys::tty::Size) -> bool {
    /// Rows the pairing screen needs, which is the symbol plus the two lines that are
    /// never given away. A version-3 symbol with its quiet zone is 37 modules, drawn one
    /// row per module, and the countdown and the way out take four rows between them.
    ///
    /// 44 rather than 50, and the four rows of slack are deliberate rather than left over.
    /// The first version asked for 50, which on a 1024x768 panel rejected **every** font --
    /// 8x16 gives 48 rows there -- and left the screen with no QR code at all. Asking for
    /// more than the layout needs does not make the layout safer; it makes the machine
    /// refuse on screens the layout would have fitted.
    const ROWS: u16 = 44;
    /// Columns for the same symbol at two cells a module, with a little either side.
    const COLUMNS: u16 = 80;

    size.rows >= ROWS && size.columns >= COLUMNS
}

/// Lowers how much the kernel prints to the screen, once there is a screen worth keeping.
///
/// `console=tty0` puts kernel messages on the foreground virtual terminal, which is this
/// one, and at the default level that includes every USB device that is plugged in or
/// wakes up. On the reference laptop a fingerprint reader re-enumerating wrote eight lines
/// across the dashboard within thirty seconds of boot.
///
/// Warnings and errors still appear, which is the level worth having: this screen is also
/// where somebody looks when the machine is unwell. Nothing is lost either way -- the ring
/// buffer keeps everything, and `dmesg` on the second terminal reads it.
///
/// Done here rather than on the kernel command line deliberately. The command line lives
/// inside a signed UKI, so a change costs a rebuild, and it would apply from the first
/// instant of boot -- silencing exactly the messages somebody needs when a boot goes wrong.
/// This happens when the dashboard starts, which is to say once the boot has gone right.
fn quieten_the_kernel(log: &mut dyn FnMut(&str)) {
    /// Print `KERN_WARNING` and worse. The default is 7, which is everything but debug.
    const WARNINGS_AND_WORSE: &str = "4\n";

    match std::fs::write("/proc/sys/kernel/printk", WARNINGS_AND_WORSE) {
        Ok(()) => log("kernel messages below warning level will stay off this screen"),
        Err(error) => log(&format!(
            "could not quieten kernel messages: {error}. Harmless: the dashboard repaints \
             over them. Remedy: echo 4 > /proc/sys/kernel/printk"
        )),
    }
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
            wireless: None,
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
    fn a_font_is_only_kept_if_the_screen_it_leaves_can_still_show_a_pairing_code() {
        // The mistake this replaces, at the sizes that made it. Biggest-font-wins was right
        // on the 2880x1620 reference laptop and wrong on a 1080p panel, where TER16x32
        // gives 33 rows and the pairing screen needs 47 -- so asking for bigger text made
        // the QR code disappear.
        use plexos_sys::tty::Size;

        // 2880x1620: TER16x32 -> 50x180. Kept.
        assert!(fits(Size {
            rows: 50,
            columns: 180
        }));
        // 1920x1080 with the same font -> 33x120. Rejected, and this is the case the
        // machine found.
        assert!(!fits(Size {
            rows: 33,
            columns: 120
        }));
        // The same panel one size down, TER10x18 -> 60x192. Kept, and still twice the area
        // of the kernel's default.
        assert!(fits(Size {
            rows: 60,
            columns: 192
        }));
        // A 1024x768 panel at 8x16 -> 48x128. **Accepted**, and the previous version of
        // this test asserted the opposite. Asking for 50 rows rejected every font on that
        // screen and left it with no QR code at all, which is not caution -- it is refusing
        // on a screen the layout fits. The pairing screen needs the symbol and four rows,
        // and sheds its optional text to make room rather than reserving a fixed block.
        assert!(fits(Size {
            rows: 48,
            columns: 128
        }));
        assert!(fits(Size {
            rows: 44,
            columns: 80
        }));
        assert!(
            !fits(Size {
                rows: 43,
                columns: 80
            }),
            "below the symbol plus the countdown and the way out there is no screen"
        );
        // And nothing absurd is accepted.
        assert!(!fits(Size {
            rows: 24,
            columns: 80
        }));
        assert!(!fits(Size {
            rows: 60,
            columns: 40
        }));
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
            going: None,
            radio: None,
            typed: String::new(),
        };

        handle(Key::Char(b'P'), &mut state, &facts_at(None), &mut silent());

        assert_eq!(state.screen, Screen::Dashboard);
        assert!(
            crate::pairing::secret().is_none(),
            "nothing was put on offer"
        );
    }

    #[test]
    fn an_arrow_key_is_an_arrow_and_not_an_escape() {
        // The defect this exists to stop, and it was found by reading the handler rather
        // than by pressing anything: the terminal sends Up as `ESC [ A`, this screen read
        // one byte at a time, and the first byte is the key that cancels. So an arrow --
        // and Home, and End, and every function key -- cancelled an active pairing offer,
        // with nothing on screen saying why the code had gone.
        let mut buffer = b"\x1b[A".to_vec();
        assert_eq!(keys_from(&mut buffer, true), vec![Key::Up]);
        assert!(buffer.is_empty(), "the whole sequence is consumed");

        for (bytes, key) in [
            (&b"\x1b[B"[..], Key::Down),
            (&b"\x1b[C"[..], Key::Right),
            (&b"\x1b[D"[..], Key::Left),
            (&b"\x1b[H"[..], Key::Home),
            (&b"\x1b[F"[..], Key::End),
            // Application mode, which some terminals use for the same keys.
            (&b"\x1bOA"[..], Key::Up),
        ] {
            let mut buffer = bytes.to_vec();
            assert_eq!(keys_from(&mut buffer, true), vec![key], "{bytes:?}");
        }
    }

    #[test]
    fn a_lone_escape_is_still_escape_once_nothing_follows_it() {
        // The one genuinely hard case: a bare Escape and the start of an arrow are the same
        // first byte, and only time tells them apart. Unsettled, it is held; settled, it is
        // the key. One tick of delay for Escape, none for a sequence.
        let mut buffer = vec![0x1b];
        assert_eq!(
            keys_from(&mut buffer, false),
            vec![],
            "held while more may arrive"
        );
        assert_eq!(buffer, vec![0x1b], "and kept, not dropped");
        assert_eq!(keys_from(&mut buffer, true), vec![Key::Escape]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_sequence_split_across_ticks_is_still_one_key() {
        // Three bytes do not have to arrive together, and on a slow console they do not.
        let mut buffer = vec![0x1b];
        assert_eq!(keys_from(&mut buffer, false), vec![]);
        buffer.push(b'[');
        assert_eq!(keys_from(&mut buffer, false), vec![]);
        buffer.push(b'B');
        assert_eq!(keys_from(&mut buffer, false), vec![Key::Down]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_function_key_is_swallowed_whole_rather_than_acted_on_in_pieces() {
        // `ESC [ 1 5 ~` is F5. Unrecognised, and consumed *entirely*: leaving the tail in
        // the buffer would make `~` the next keystroke, and leaving the `ESC` would cancel.
        let mut buffer = b"\x1b[15~p".to_vec();
        assert_eq!(
            keys_from(&mut buffer, true),
            vec![Key::Char(b'p')],
            "the function key does nothing and the key after it is untouched"
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn ordinary_keys_are_unaffected_and_arrive_in_order() {
        let mut buffer = b"pdq".to_vec();
        assert_eq!(
            keys_from(&mut buffer, true),
            vec![Key::Char(b'p'), Key::Char(b'd'), Key::Char(b'q')]
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
            going: None,
            radio: None,
            typed: String::new(),
        };

        handle(Key::Char(b'p'), &mut state, &facts, &mut silent());
        assert!(crate::pairing::secret().is_some(), "a code is on offer");

        advance(&mut state, &facts, &crate::wifi::Progress::default());
        let Screen::Pairing { url, .. } = &state.screen else {
            panic!("expected the pairing screen, got {:?}", state.screen);
        };
        assert!(url.contains("#pair="), "{url}");

        handle(Key::Escape, &mut state, &facts, &mut silent());
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
            going: None,
            radio: None,
            typed: String::new(),
        };
        crate::pairing::consume(&secret).expect("the browser spends it");
        advance(&mut state, &facts, &crate::wifi::Progress::default());
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
            going: None,
            radio: None,
            typed: String::new(),
        };

        handle(Key::Char(b'x'), &mut state, &facts, &mut silent());

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
            going: None,
            radio: None,
            typed: String::new(),
        };

        // No cable yet: nothing is offered, and the screen says what it is waiting for.
        advance(
            &mut state,
            &facts_at(None),
            &crate::wifi::Progress::default(),
        );
        assert!(crate::pairing::secret().is_none());
        let Screen::FirstBoot { url, .. } = &state.screen else {
            panic!("expected the first-boot screen");
        };
        assert!(url.is_none());

        // The lease arrives.
        advance(
            &mut state,
            &facts_at(Some("192.168.2.102")),
            &crate::wifi::Progress::default(),
        );
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
        advance(
            &mut state,
            &facts_at(Some("192.168.2.102")),
            &crate::wifi::Progress::default(),
        );
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
            going: None,
            radio: None,
            typed: String::new(),
        };

        advance(&mut state, &facts, &crate::wifi::Progress::default());
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
            going: None,
            radio: None,
            typed: String::new(),
        };

        handle(Key::Char(b'd'), &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Details);
        handle(Key::Escape, &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Dashboard);

        handle(Key::Char(b'?'), &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Help);
        handle(Key::Escape, &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Dashboard);
    }

    /// The dashboard, with nothing pending.
    fn at_dashboard() -> State {
        State {
            screen: Screen::Dashboard,
            notice_until: None,
            first_boot_code: None,
            first_boot_offered: true,
            going: None,
            radio: None,
            typed: String::new(),
        }
    }

    #[test]
    fn stopping_the_machine_takes_three_keys_and_no_two_of_them_are_the_same() {
        // This screen stands in a room. Anybody who walks past it can press a key, and the
        // thing on the other side of these keys is a media server going dark in the middle
        // of whatever somebody is watching -- so the count is the feature. O opens a menu,
        // Enter takes the row, Y answers a question that names the outcome.
        use plexos_sys::power::Action;
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = at_dashboard();

        handle(Key::Char(b'o'), &mut state, &facts, &mut silent());
        assert_eq!(
            state.screen,
            Screen::Power {
                choice: Action::Restart
            },
            "restart is under the cursor, because it is the one that leaves the machine \
             reachable afterwards"
        );
        assert!(state.going.is_none(), "opening a menu stops nothing");

        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        assert_eq!(
            state.screen,
            Screen::PowerConfirm {
                choice: Action::Restart
            }
        );
        assert!(state.going.is_none(), "and neither does choosing a row");

        // Enter again is *not* the answer. It is the key that got somebody here, so a
        // confirmation that took it would turn two presses of one key into a shutdown.
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        assert!(
            state.going.is_none(),
            "Enter must not confirm what Enter asked"
        );

        handle(Key::Char(b'y'), &mut state, &facts, &mut silent());
        assert_eq!(state.going, Some(Action::Restart));
        assert_eq!(
            state.screen,
            Screen::PowerGoing {
                choice: Action::Restart
            }
        );
    }

    #[test]
    fn the_arrows_choose_between_the_two_and_wrap_rather_than_stopping() {
        // Two rows, so either arrow is the other one. A list of two where Up does nothing on
        // the first row is a list that appears not to answer half the presses.
        use plexos_sys::power::Action;
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = at_dashboard();
        handle(Key::Char(b'O'), &mut state, &facts, &mut silent());

        for key in [Key::Down, Key::Up, Key::Down] {
            let before = match state.screen {
                Screen::Power { choice } => choice,
                ref other => panic!("expected the power menu, got {other:?}"),
            };
            handle(key, &mut state, &facts, &mut silent());
            let after = match state.screen {
                Screen::Power { choice } => choice,
                ref other => panic!("expected the power menu, got {other:?}"),
            };
            assert_ne!(before, after, "{key:?} moved nothing");
        }
        assert_eq!(
            state.screen,
            Screen::Power {
                choice: Action::Off
            }
        );

        // And the confirmation asks about the row that was actually under the cursor.
        handle(Key::Char(b'\n'), &mut state, &facts, &mut silent());
        handle(Key::Char(b'Y'), &mut state, &facts, &mut silent());
        assert_eq!(state.going, Some(Action::Off));
    }

    #[test]
    fn escape_backs_out_of_every_step_and_stops_nothing() {
        // The way out has to exist at each step, and the step it goes back to matters:
        // somebody who answered "no" to shutting down may well have meant restart, and
        // sending them to the dashboard to press O again is the machine being pedantic.
        use plexos_sys::power::Action;
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = at_dashboard();

        handle(Key::Char(b'o'), &mut state, &facts, &mut silent());
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        handle(Key::Escape, &mut state, &facts, &mut silent());
        assert_eq!(
            state.screen,
            Screen::Power {
                choice: Action::Restart
            },
            "back to the menu, not out of it"
        );

        handle(Key::Escape, &mut state, &facts, &mut silent());
        assert_eq!(state.screen, Screen::Dashboard);
        assert!(state.going.is_none(), "nothing was ever asked for");
    }

    #[test]
    fn a_machine_on_its_way_down_does_not_take_the_key_back() {
        // `PowerGoing` is painted and then the sequence starts. A key that appeared to
        // cancel it would be the screen lying: `stop_now` does not return.
        use plexos_sys::power::Action;
        let facts = facts_at(Some("192.168.2.102"));
        let mut state = at_dashboard();
        handle(Key::Char(b'o'), &mut state, &facts, &mut silent());
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        handle(Key::Char(b'y'), &mut state, &facts, &mut silent());
        state.going.take();

        for key in [
            Key::Escape,
            Key::Char(b'q'),
            Key::Char(b'n'),
            Key::Up,
            Key::Char(b'p'),
        ] {
            handle(key, &mut state, &facts, &mut silent());
            assert_eq!(
                state.screen,
                Screen::PowerGoing {
                    choice: Action::Restart
                },
                "{key:?} pretended to take it back"
            );
        }
    }

    #[test]
    fn no_key_on_the_dashboard_stops_the_machine() {
        // The property the three-key count exists for, asserted over the whole alphabet
        // rather than over the keys this file happens to name. A key added here later that
        // reached `going` in one press would fail this.
        //
        // Locked because `p` is in the range and `p` puts a pairing code on offer, which is
        // global to the process -- and Rust runs tests as threads in one process.
        let _serialised = crate::pairing::test_lock();
        let facts = facts_at(Some("192.168.2.102"));
        for byte in 0_u8..=127 {
            let mut state = at_dashboard();
            handle(Key::Char(byte), &mut state, &facts, &mut silent());
            assert!(
                state.going.is_none(),
                "{:?} stopped the machine on its own",
                byte as char
            );
        }
        for key in [Key::Escape, Key::Up, Key::Down, Key::Left, Key::Right] {
            let mut state = at_dashboard();
            handle(key, &mut state, &facts, &mut silent());
            assert!(state.going.is_none(), "{key:?} stopped the machine");
        }
    }

    /// A machine with a radio fitted.
    fn with_a_radio(address: Option<&str>) -> Facts {
        Facts {
            wireless: Some("wlan0".to_owned()),
            ..facts_at(address)
        }
    }

    /// What the radio's job looks like after a scan that found these.
    fn found(networks: &[(&str, f32, crate::wifi::Security)]) -> crate::wifi::Progress {
        crate::wifi::Progress {
            networks: networks
                .iter()
                .map(|(ssid, dbm, security)| crate::wifi::Network {
                    ssid: (*ssid).to_owned(),
                    bssid: "00:11:22:33:44:55".to_owned(),
                    signal_dbm: *dbm,
                    frequency_mhz: 5180,
                    security: *security,
                    hidden: false,
                })
                .collect(),
            ..crate::wifi::Progress::default()
        }
    }

    #[test]
    fn pressing_w_on_a_machine_with_no_radio_does_nothing_at_all() {
        // The same rule as P with no address, and it matters more here: a list that can
        // never have anything in it is a key somebody presses, sees nothing happen, and
        // concludes the appliance is broken.
        let mut state = at_dashboard();
        handle(Key::Char(b'W'), &mut state, &facts_at(None), &mut silent());
        assert_eq!(state.screen, Screen::Dashboard);
        assert!(state.radio.is_none(), "and nothing touched the radio");
    }

    #[test]
    fn the_list_fills_itself_in_from_the_scan_with_nobody_pressing_anything() {
        // A scan takes seconds. Somebody who pressed W is looking at the screen for the
        // whole of it, so the list appearing has to be something the loop does rather than
        // something a keystroke asks for.
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        let mut state = at_dashboard();

        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        assert!(
            matches!(state.radio, Some(Radio::Scan)),
            "pressing W starts a scan without being asked twice"
        );
        assert_eq!(state.screen, empty_list());

        advance(&mut state, &facts, &found(&[]));
        let Screen::Wireless { rows, .. } = &state.screen else {
            panic!("expected the list, got {:?}", state.screen);
        };
        assert!(rows.is_empty());

        advance(
            &mut state,
            &facts,
            &found(&[
                ("Upstairs", -45.0, Security::Psk),
                ("Shed", -80.0, Security::Sae),
            ]),
        );
        let Screen::Wireless { rows, .. } = &state.screen else {
            panic!("expected the list");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ssid, "Upstairs");
        assert!(
            rows[0].bars > rows[1].bars,
            "a stronger signal draws more bars: {rows:?}"
        );
    }

    #[test]
    fn a_passphrase_reaches_the_supplicant_and_nothing_else() {
        // The property this whole arrangement exists for. What somebody types is held in one
        // field of the keyboard's own state, moved into the message that starts the join,
        // and is gone from both by the time anything paints a screen.
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[("Upstairs", -45.0, Security::Psk)]),
        );

        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        assert_eq!(
            state.screen,
            Screen::WirelessKey {
                ssid: "Upstairs".to_owned(),
                typed: 0,
                open: false,
            }
        );

        for byte in b"hunter2 " {
            handle(Key::Char(*byte), &mut state, &facts, &mut silent());
        }
        let Screen::WirelessKey { typed, .. } = state.screen else {
            panic!("expected the passphrase screen");
        };
        assert_eq!(typed, 8, "the count follows what is held");
        assert_eq!(state.typed, "hunter2 ");

        state.radio.take();
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        let Some(Radio::Join {
            ssid,
            passphrase,
            security,
        }) = state.radio.take()
        else {
            panic!("nothing was asked of the radio");
        };
        assert_eq!(ssid, "Upstairs");
        assert_eq!(passphrase, "hunter2 ");
        assert_eq!(security, Security::Psk);
        assert!(
            state.typed.is_empty(),
            "it is moved into the message rather than copied out of a field that keeps it"
        );
    }

    #[test]
    fn the_passphrase_is_not_in_the_thing_that_paints_the_screen() {
        // Asserted against the rendered frame rather than against the type, because the type
        // being unable to hold it is exactly the claim being checked -- and a claim about a
        // type is worth nothing if the value reaches the screen by another route.
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[("Upstairs", -45.0, Security::Psk)]),
        );
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        for byte in b"correcthorse" {
            handle(Key::Char(*byte), &mut state, &facts, &mut silent());
        }

        let painted = render::frame(&state.screen, &facts, 50, 180);
        assert!(
            !painted.contains("correcthorse"),
            "the passphrase reached the screen:\n{painted}"
        );
        assert!(
            painted.contains("************"),
            "and the mask is as long as what was typed:\n{painted}"
        );
    }

    #[test]
    fn escaping_out_of_the_passphrase_takes_it_out_of_memory_too() {
        // A passphrase left behind because somebody changed their mind is one nobody knows
        // is there. It is also how the next network typed into this screen would start with
        // the previous one's characters already counted.
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[("Upstairs", -45.0, Security::Psk)]),
        );
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        for byte in b"abcdef" {
            handle(Key::Char(*byte), &mut state, &facts, &mut silent());
        }

        handle(Key::Escape, &mut state, &facts, &mut silent());
        assert!(state.typed.is_empty());
        assert!(matches!(state.screen, Screen::Wireless { .. }));
    }

    #[test]
    fn backspace_corrects_whichever_byte_the_terminal_sends_for_it() {
        // The Linux console sends DEL and a great many terminals send BS. Accepting one is
        // a screen where the correction key does nothing on somebody else's machine.
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        for erase in [0x7f_u8, 0x08] {
            let mut state = at_dashboard();
            handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
            advance(
                &mut state,
                &facts,
                &found(&[("Upstairs", -45.0, Security::Psk)]),
            );
            handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
            for byte in b"abc" {
                handle(Key::Char(*byte), &mut state, &facts, &mut silent());
            }
            handle(Key::Char(erase), &mut state, &facts, &mut silent());

            assert_eq!(state.typed, "ab", "{erase:#04x} did not erase");
            let Screen::WirelessKey { typed, .. } = state.screen else {
                panic!("expected the passphrase screen");
            };
            assert_eq!(typed, 2, "and the mask followed it");
        }
    }

    #[test]
    fn an_empty_passphrase_is_refused_here_rather_than_by_the_access_point() {
        // The supplicant's answer to no passphrase is twenty-five seconds of retrying and
        // then a timeout, which is the same screen a *wrong* passphrase produces. Refusing
        // it here is the difference between "you typed nothing" and "it did not work".
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[("Upstairs", -45.0, Security::Psk)]),
        );
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        state.radio.take();

        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        assert!(state.radio.is_none(), "nothing was sent");
        assert!(matches!(state.screen, Screen::WirelessKey { .. }));

        // An open network is the other way round: there is nothing to type, so Enter is the
        // whole interaction.
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[("CoffeeShop", -60.0, Security::Open)]),
        );
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        state.radio.take();
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        assert!(
            matches!(state.radio, Some(Radio::Join { ref passphrase, .. }) if passphrase.is_empty()),
            "an open network joins with no credential"
        );
    }

    #[test]
    fn a_network_this_appliance_cannot_join_says_why_instead_of_trying() {
        // Both refusals name what to change on the access point, which is the only place
        // either can be fixed. A screen that said "could not join" would send somebody to
        // look at the appliance.
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        for security in [Security::Wep, Security::Enterprise] {
            let mut state = at_dashboard();
            handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
            advance(
                &mut state,
                &facts,
                &found(&[("OldRouter", -50.0, security)]),
            );
            state.radio.take();

            handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
            let Screen::Wireless { note, .. } = &state.screen else {
                panic!("expected to stay on the list, got {:?}", state.screen);
            };
            let note = note.as_deref().unwrap_or_default();
            assert!(note.contains("Remedy"), "{security:?}: {note}");
            assert!(state.radio.is_none(), "{security:?} was tried anyway");
        }
    }

    #[test]
    fn a_scan_that_comes_back_shorter_does_not_move_somebody_s_cursor_off_the_end() {
        // Networks come and go between scans. A cursor left past the end is a join that
        // reads a row that is not there, and a cursor reset to the top is somebody losing
        // the network they had just found in a list of twenty.
        use crate::wifi::Security;
        let facts = with_a_radio(None);
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[
                ("A", -40.0, Security::Psk),
                ("B", -50.0, Security::Psk),
                ("C", -60.0, Security::Psk),
            ]),
        );
        handle(Key::End, &mut state, &facts, &mut silent());
        assert!(matches!(state.screen, Screen::Wireless { choice: 2, .. }));

        advance(&mut state, &facts, &found(&[("A", -40.0, Security::Psk)]));
        let Screen::Wireless { choice, rows, .. } = &state.screen else {
            panic!("expected the list");
        };
        assert_eq!(*choice, 0);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn connecting_ends_on_a_screen_that_says_so() {
        use crate::wifi::{Phase, Progress, Security};
        let facts = with_a_radio(None);
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[("Upstairs", -45.0, Security::Psk)]),
        );
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        for byte in b"passphrase" {
            handle(Key::Char(*byte), &mut state, &facts, &mut silent());
        }
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());

        // Associating: the radio's own words, so a screen and a page reading the same job
        // cannot disagree about where it has got to.
        advance(
            &mut state,
            &facts,
            &Progress {
                phase: Phase::Addressing,
                detail: "asking for an address".to_owned(),
                ..Progress::default()
            },
        );
        assert_eq!(
            state.screen,
            Screen::WirelessJoining {
                ssid: "Upstairs".to_owned(),
                detail: "asking for an address".to_owned(),
                error: None,
            }
        );

        advance(
            &mut state,
            &facts,
            &Progress {
                phase: Phase::Connected,
                detail: "connected to Upstairs".to_owned(),
                ..Progress::default()
            },
        );
        assert_eq!(
            state.screen,
            Screen::WirelessJoined {
                ssid: "Upstairs".to_owned()
            }
        );
        assert!(state.notice_until.is_some(), "and it gives way on its own");
    }

    #[test]
    fn a_failed_join_keeps_the_reason_on_the_screen_somebody_is_standing_at() {
        // This is the screen somebody is at *because* the browser is not reachable. A
        // summary here sends them to a console they cannot open.
        use crate::wifi::{Phase, Progress, Security};
        let facts = with_a_radio(None);
        let mut state = at_dashboard();
        handle(Key::Char(b'w'), &mut state, &facts, &mut silent());
        advance(
            &mut state,
            &facts,
            &found(&[("Upstairs", -45.0, Security::Psk)]),
        );
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());
        for byte in b"wrong" {
            handle(Key::Char(*byte), &mut state, &facts, &mut silent());
        }
        handle(Key::Char(b'\r'), &mut state, &facts, &mut silent());

        let excuse = "the network did not accept it. Remedy: check the passphrase.";
        advance(
            &mut state,
            &facts,
            &Progress {
                phase: Phase::Failed,
                error: Some(excuse.to_owned()),
                ..Progress::default()
            },
        );
        let painted = render::frame(&state.screen, &facts, 50, 180);
        assert!(painted.contains("Remedy"), "{painted}");

        // And the way back is to the list, so a second attempt is one keystroke rather than
        // starting from the dashboard.
        handle(Key::Escape, &mut state, &facts, &mut silent());
        assert!(matches!(state.screen, Screen::Wireless { .. }));
    }

    #[test]
    fn the_radio_disappearing_takes_the_screen_with_it() {
        // A USB adapter can be unplugged while somebody is reading the list. Left alone, the
        // screen would sit on networks it can no longer see, with a join key that could
        // never do anything.
        use crate::wifi::Security;
        let mut state = at_dashboard();
        handle(
            Key::Char(b'w'),
            &mut state,
            &with_a_radio(None),
            &mut silent(),
        );
        advance(
            &mut state,
            &with_a_radio(None),
            &found(&[("Upstairs", -45.0, Security::Psk)]),
        );
        handle(
            Key::Char(b'\r'),
            &mut state,
            &with_a_radio(None),
            &mut silent(),
        );
        for byte in b"secret" {
            handle(
                Key::Char(*byte),
                &mut state,
                &with_a_radio(None),
                &mut silent(),
            );
        }

        advance(
            &mut state,
            &facts_at(None),
            &crate::wifi::Progress::default(),
        );
        assert_eq!(state.screen, Screen::Dashboard);
        assert!(
            state.typed.is_empty(),
            "and it does not leave a passphrase behind"
        );
    }

    #[test]
    fn anything_in_range_gets_at_least_one_bar() {
        // Zero bars beside a network the scan has just found reads as a fault rather than as
        // a weak signal. The scan found it, so something reached this machine.
        assert_eq!(bars_for(0.0), 1);
        assert_eq!(bars_for(1.0), render::BARS);
        assert!(bars_for(0.5) > 1 && bars_for(0.5) < render::BARS);
        // And nothing outside the range, whatever a driver reports.
        assert_eq!(bars_for(-3.0), 1);
        assert_eq!(bars_for(9.0), render::BARS);
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
            going: None,
            radio: None,
            typed: String::new(),
        };

        for key in b"sSfF0123456789\t\r\n" {
            handle(Key::Char(*key), &mut state, &facts, &mut silent());
            assert_eq!(
                state.screen,
                Screen::Dashboard,
                "{:?} moved the screen somewhere",
                *key as char
            );
        }
    }
}
