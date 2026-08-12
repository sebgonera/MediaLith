//! Turning the model into the bytes a terminal paints (ADR-0019).
//!
//! Pure, and that is the point: [`frame`] takes a screen and a size and returns a string.
//! Nothing here opens a device, reads a clock or asks the machine anything, so every state
//! this appliance can be in — recovered, on trial, no network, pairing, expired — is a test
//! that runs on a build host in microseconds.
//!
//! # It repaints the whole screen, every time
//!
//! No diffing, no cursor arithmetic beyond "go to the top". A frame is a few kilobytes and
//! the alternative is a renderer that has to know what was there before, which is the class
//! of bug that leaves half a QR code on screen after a state change.
//!
//! What *is* diffed is the frame itself, by the caller: an identical frame is not written
//! at all. That is what lets an untouched appliance fall silent so the kernel's blank timer
//! can turn the panel off — see [`super::model::coarse`] for why the uptime on this screen
//! is deliberately coarse.
//!
//! # Colour is never the only signal
//!
//! Every mark has a word beside it. The panel this is read on is a console with eight
//! colours, photographed for a forum post as often as it is looked at, and read by whoever
//! is standing in the room.

use plexos_sys::power::Action;

use super::model::{Facts, Mark, Plex, Verdict, coarse};
use super::qr::Symbol;

/// What the attached screen is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// The ordinary state: what this machine is and whether it is working.
    Dashboard,
    /// A pairing code, as a QR symbol and a countdown.
    Pairing {
        /// The URL encoded in the symbol.
        url: String,
        /// Whole seconds left before it expires.
        seconds_left: u64,
    },
    /// The code just expired, and nothing else has happened since.
    PairingExpired,
    /// A browser has just paired.
    Paired,
    /// A machine being set up for the first time, with its recovery code on screen.
    FirstBoot {
        /// The URL encoded in the symbol, when there is a network to reach it on.
        url: Option<String>,
        /// The recovery device code, in the form a person reads.
        recovery_code: String,
    },
    /// Everything the dashboard knows, at length.
    Details,
    /// What the keys do.
    Help,
    /// The two ways to stop the machine, with the cursor on one of them.
    Power {
        /// Which one the cursor is on.
        choice: Action,
    },
    /// The chosen one, asked about once more before it happens.
    ///
    /// A separate screen rather than a line on the one above, because this screen stands in
    /// a room and anybody who walks past it can press a key. One keystroke between a
    /// dashboard and a media server going dark is not a margin.
    PowerConfirm {
        /// What is about to happen.
        choice: Action,
    },
    /// It has been asked for, and this is the last frame this screen will draw.
    PowerGoing {
        /// What is happening.
        choice: Action,
    },
}

/// How wide the laid-out content is, whatever the screen's own width.
///
/// A dashboard stretched across 360 columns is a dashboard nobody can read: the eye has to
/// travel the width of a 13-inch panel between a label and its value. So the content is a
/// fixed column, centred — which is the same decision the console page makes with a
/// `max-width`, for the same reason.
const CONTENT: usize = 76;

/// Escape sequences, kept together so that the vocabulary of this file is visible in one
/// place rather than scattered through the layout as magic strings.
mod sgr {
    /// Home the cursor and clear everything.
    pub const CLEAR: &str = "\x1b[H\x1b[2J";
    /// Hide the cursor. A block cursor parked in the middle of a dashboard is the one
    /// detail that makes a designed screen look like a prompt somebody walked away from.
    pub const HIDE_CURSOR: &str = "\x1b[?25l";
    /// Back to the terminal's defaults.
    pub const RESET: &str = "\x1b[0m";
    /// Emphasis.
    pub const BOLD: &str = "\x1b[1m";
    /// Secondary text.
    pub const DIM: &str = "\x1b[2m";
    /// Working.
    pub const GREEN: &str = "\x1b[32m";
    /// Needs attention.
    pub const YELLOW: &str = "\x1b[33m";
    /// In progress, or waiting for somebody.
    pub const CYAN: &str = "\x1b[36m";
}

/// One line of the screen: what to write, and how wide it looks once written.
///
/// The two are separate because a line carrying colour is longer than it looks, and
/// centring on its byte length would drift further right the more colour it had. That is
/// the same mistake as measuring a rendered page by its markup.
#[derive(Debug, Clone)]
struct Line {
    text: String,
    width: usize,
}

impl Line {
    fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        let width = text.chars().count();
        Self { text, width }
    }

    fn blank() -> Self {
        Self::plain("")
    }

    /// A line whose visible width is known but whose text carries escapes.
    fn drawn(text: impl Into<String>, width: usize) -> Self {
        Self {
            text: text.into(),
            width,
        }
    }

    /// A label on the left of a fixed column and a value on the right of it.
    fn field(label: &str, value: &str) -> Self {
        let gap = CONTENT.saturating_sub(label.chars().count() + value.chars().count());
        Self::plain(format!("{label}{}{value}", " ".repeat(gap.max(2))))
    }
}

/// The whole screen, ready to write.
///
/// Deterministic: the same model and the same size produce byte-identical output. The
/// caller relies on that to decide not to write at all.
#[must_use]
pub fn frame(screen: &Screen, facts: &Facts, rows: usize, columns: usize) -> String {
    let lines = match screen {
        Screen::Dashboard => dashboard(facts),
        Screen::Pairing { url, seconds_left } => pairing(url, *seconds_left, rows, columns),
        Screen::PairingExpired => pairing_expired(),
        Screen::Paired => paired(),
        Screen::FirstBoot { url, recovery_code } => {
            first_boot(facts, url.as_deref(), recovery_code, rows, columns)
        }
        Screen::Details => details(facts),
        Screen::Help => help(),
        Screen::Power { choice } => power(*choice, facts),
        Screen::PowerConfirm { choice } => power_confirm(*choice, facts),
        Screen::PowerGoing { choice } => power_going(*choice),
    };

    place(&lines, rows, columns)
}

/// Centres the block on the screen, horizontally and vertically.
///
/// Vertically as well, because a dashboard hugging the top of a 101-row panel with
/// eighty rows of black underneath it reads as a machine that has stopped printing rather
/// than as a screen that is finished.
///
/// A block taller than the screen is drawn from the top and allowed to run off the bottom.
/// Nothing this file produces is that tall on any real console, and the alternative --
/// dropping lines to make it fit -- would silently remove whichever fact came last.
fn place(lines: &[Line], rows: usize, columns: usize) -> String {
    let mut out = String::with_capacity(columns * lines.len() + 64);
    out.push_str(sgr::HIDE_CURSOR);
    out.push_str(sgr::CLEAR);

    let top = rows.saturating_sub(lines.len()) / 2;
    for _ in 0..top {
        out.push_str("\r\n");
    }

    for line in lines {
        let left = columns.saturating_sub(line.width) / 2;
        out.push_str(&" ".repeat(left));
        out.push_str(&line.text);
        out.push_str(sgr::RESET);
        out.push_str("\r\n");
    }

    out
}

/// The wordmark, which every screen opens with.
fn wordmark() -> Vec<Line> {
    vec![
        Line::drawn(
            format!("{}{}MediaLith{}", sgr::BOLD, sgr::RESET, sgr::RESET),
            9,
        ),
        Line::drawn(
            format!("{}Home Media Appliance{}", sgr::DIM, sgr::RESET),
            20,
        ),
    ]
}

/// The headline, with its mark.
fn headline(verdict: &Verdict) -> Line {
    let (mark, said) = verdict.headline();
    let (colour, symbol) = match mark {
        Mark::Good => (sgr::GREEN, "*"),
        Mark::Warning => (sgr::YELLOW, "!"),
        Mark::Testing => (sgr::CYAN, "~"),
    };
    // The symbol is ASCII rather than a bullet, and that is the same decision the QR
    // renderer makes: this image's console font is whatever the kernel loaded, and a
    // missing glyph is a blank cell that looks like a bug rather than like a font.
    Line::drawn(
        format!("{colour}{}{symbol}  {said}{}", sgr::BOLD, sgr::RESET),
        said.chars().count() + 3,
    )
}

fn dashboard(facts: &Facts) -> Vec<Line> {
    let mut lines = wordmark();
    lines.push(Line::blank());
    lines.push(headline(&facts.verdict));
    lines.push(Line::blank());
    lines.push(Line::blank());

    lines.push(Line::field("Plex Media Server", facts.plex.word()));
    lines.push(Line::field(
        "Hardware transcoding",
        facts.transcoding.word(),
    ));
    lines.push(Line::field(
        "Network",
        &match (facts.address(), facts.interface.as_deref()) {
            (Some(address), Some(interface)) => format!("{address}  ({interface})"),
            (Some(address), None) => address.to_owned(),
            (None, _) => "no address".to_owned(),
        },
    ));

    lines.push(Line::blank());
    lines.push(Line::blank());

    if let Some(address) = facts.address() {
        lines.push(Line::drawn(
            format!("{}https://{address}{}", sgr::BOLD, sgr::RESET),
            address.chars().count() + 8,
        ));
        lines.push(Line::blank());
        lines.push(Line::plain("Press P to pair a browser"));
    } else {
        {
            // No misleading invitation. Pressing P on a machine with no address would
            // produce a QR code pointing nowhere, so the screen does not offer it.
            lines.push(Line::plain("Waiting for a network address..."));
            lines.push(Line::blank());
            lines.push(Line::drawn(
                format!(
                    "{}Connect this machine to your network with a cable.{}",
                    sgr::DIM,
                    sgr::RESET
                ),
                50,
            ));
        }
    }

    lines.push(Line::blank());
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!("{}{}{}", sgr::DIM, identity(facts), sgr::RESET),
        identity(facts).chars().count(),
    ));
    lines.push(Line::blank());
    lines.push(footer(facts.address().is_some()));
    lines
}

/// The version, slot and uptime line.
fn identity(facts: &Facts) -> String {
    let mut parts = vec![facts.product.clone()];
    if let Some(slot) = &facts.slot {
        parts.push(format!("Slot {}", slot.to_uppercase()));
    }
    if let Some(uptime) = facts.uptime {
        parts.push(format!("up {}", coarse(uptime)));
    }
    parts.join("  ·  ")
}

/// The keys, and only the ones that do something.
fn footer(can_pair: bool) -> Line {
    let mut keys = Vec::new();
    if can_pair {
        keys.push("P  Pair a browser");
    }
    keys.push("D  Details");
    keys.push("O  Power");
    keys.push("?  Help");
    let text = keys.join("     ");
    Line::drawn(
        format!("{}{text}{}", sgr::DIM, sgr::RESET),
        text.chars().count(),
    )
}

/// The pairing screen: a symbol, a countdown, and the way out.
///
/// **The symbol comes first and everything else is optional.** That ordering is the whole
/// of this function, and it is a correction: the first version reserved ten rows for text
/// and gave the symbol what was left, so on a short screen it reported that it had no room
/// — which is a pairing screen with no pairing code on it, on the one machine whose owner
/// was standing in front of it.
///
/// So the room is spent in order of what somebody came here for. The symbol, then the
/// countdown, then the way out, then the wordmark and the explanations. A screen with
/// thirty rows shows a QR code and a timer; a screen with sixty shows all of it.
fn pairing(url: &str, seconds_left: u64, rows: usize, columns: usize) -> Vec<Line> {
    /// Rows that are never given away: the countdown and the way out, with a blank line
    /// above each. Below this there is no screen worth drawing.
    const ESSENTIAL: usize = 4;

    let symbol = match symbol_rows(url, rows.saturating_sub(ESSENTIAL), columns) {
        Ok(drawn) => drawn,
        Err(problem) => {
            return vec![
                Line::drawn(format!("{}PAIR A BROWSER{}", sgr::BOLD, sgr::RESET), 14),
                Line::blank(),
                Line::plain(problem),
                Line::blank(),
                Line::plain("Use the recovery device code in the browser instead."),
            ];
        }
    };

    // What the symbol did not take. Everything below is added only while there is room, and
    // in the order it is worth having.
    let mut spare = rows.saturating_sub(symbol.len() + ESSENTIAL);
    let mut afford = |cost: usize| {
        let can = spare >= cost;
        if can {
            spare -= cost;
        }
        can
    };

    let mut lines = Vec::new();
    if afford(2) {
        lines.push(Line::drawn(
            format!("{}PAIR A BROWSER{}", sgr::BOLD, sgr::RESET),
            14,
        ));
        lines.push(Line::blank());
    }
    lines.extend(symbol);
    lines.push(Line::blank());

    if afford(1) {
        lines.push(Line::plain("Scan to administer this appliance"));
    }
    if afford(1) {
        lines.push(Line::blank());
    }

    lines.push(Line::drawn(
        format!(
            "{}{}Expires in {}{}",
            sgr::BOLD,
            sgr::CYAN,
            countdown(seconds_left),
            sgr::RESET
        ),
        11 + countdown(seconds_left).chars().count(),
    ));
    if afford(1) {
        lines.push(Line::drawn(
            format!("{}Single use{}", sgr::DIM, sgr::RESET),
            10,
        ));
    }
    if afford(1) {
        lines.push(Line::blank());
    }
    lines.push(Line::drawn(
        format!("{}ESC  cancel      P  new code{}", sgr::DIM, sgr::RESET),
        27,
    ));
    lines
}

/// `m:ss`, which is what a countdown looks like everywhere else a person has seen one.
fn countdown(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The symbol as lines, or why there is not one.
fn symbol_rows(url: &str, rows: usize, columns: usize) -> Result<Vec<Line>, String> {
    // `rows` is what the caller has already decided the symbol may have, not the height of
    // the screen. That is the correction: reserving a fixed block for text and giving the
    // symbol the remainder is how a pairing screen came to have no pairing code on it.
    let symbol = Symbol::encode(url)?;
    let scale = symbol
        .scale_for(rows, columns)
        .ok_or_else(|| "This screen is too small to show a pairing code.".to_owned())?;

    let width = symbol.drawn_width() * super::qr::CELLS_PER_MODULE * scale;
    Ok(symbol
        .rows(scale)
        .into_iter()
        .map(|row| Line::drawn(row, width))
        .collect())
}

fn pairing_expired() -> Vec<Line> {
    let mut lines = wordmark();
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!(
            "{}{}!  Pairing code expired{}",
            sgr::YELLOW,
            sgr::BOLD,
            sgr::RESET
        ),
        24,
    ));
    lines.push(Line::blank());
    lines.push(Line::plain(
        "It was good for five minutes and was not used.",
    ));
    lines.push(Line::blank());
    lines.push(Line::plain("Press P to show another."));
    lines
}

fn paired() -> Vec<Line> {
    let mut lines = wordmark();
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!("{}{}*  Browser paired{}", sgr::GREEN, sgr::BOLD, sgr::RESET),
        19,
    ));
    lines.push(Line::blank());
    lines.push(Line::plain(
        "That browser is now administering this appliance.",
    ));
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!(
            "{}The code it used is spent. Press P for another.{}",
            sgr::DIM,
            sgr::RESET
        ),
        46,
    ));
    lines
}

/// First boot: the one moment the recovery device code exists in a readable form.
fn first_boot(
    facts: &Facts,
    url: Option<&str>,
    recovery_code: &str,
    rows: usize,
    columns: usize,
) -> Vec<Line> {
    let mut lines = wordmark();
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!("{}Welcome.{}", sgr::BOLD, sgr::RESET),
        8,
    ));
    lines.push(Line::blank());

    match url {
        Some(url) => {
            lines.push(Line::plain("Scan to open setup:"));
            lines.push(Line::blank());
            // Twelve rows of welcome, recovery code and instructions surround it here,
            // and unlike the pairing screen those are the point -- the code is the one
            // thing this screen exists to show. So the symbol takes what is left.
            match symbol_rows(url, rows.saturating_sub(14), columns) {
                Ok(drawn) => lines.extend(drawn),
                Err(problem) => lines.push(Line::plain(problem)),
            }
        }
        // No symbol, and the two reasons need different sentences: a machine with no
        // address cannot be reached at all yet, while one whose code has simply run out
        // needs a keystroke. Telling somebody to check a cable that is plugged in is the
        // `operstate` mistake this project already recorded, in a friendlier place.
        None if facts.address().is_none() => {
            lines.push(Line::plain("Waiting for a network address..."));
            lines.push(Line::blank());
            lines.push(Line::drawn(
                format!(
                    "{}Connect a network cable and this will become a QR code.{}",
                    sgr::DIM,
                    sgr::RESET
                ),
                54,
            ));
        }
        None => {
            lines.push(Line::plain("The setup code has expired."));
            lines.push(Line::blank());
            lines.push(Line::drawn(
                format!("{}Press P to show another QR code.{}", sgr::DIM, sgr::RESET),
                31,
            ));
        }
    }

    lines.push(Line::blank());
    lines.push(Line::blank());
    lines.push(Line::plain("Recovery device code:"));
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!("{}{recovery_code}{}", sgr::BOLD, sgr::RESET),
        recovery_code.chars().count(),
    ));
    lines.push(Line::blank());
    lines.push(Line::plain("Write this down and keep it somewhere safe."));
    lines.push(Line::drawn(
        format!(
            "{}It is shown once. This machine keeps no copy it can read back,{}",
            sgr::DIM,
            sgr::RESET
        ),
        62,
    ));
    lines.push(Line::drawn(
        format!(
            "{}and it is the way in if the QR code is not available.{}",
            sgr::DIM,
            sgr::RESET
        ),
        53,
    ));
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!(
            "{}Press any key when you have written it down.{}",
            sgr::DIM,
            sgr::RESET
        ),
        44,
    ));
    lines
}

fn details(facts: &Facts) -> Vec<Line> {
    let mut lines = vec![
        Line::drawn(format!("{}DETAILS{}", sgr::BOLD, sgr::RESET), 7),
        Line::blank(),
    ];

    lines.push(Line::field("Release", &facts.product));
    lines.push(Line::field(
        "Version",
        facts.version.as_deref().unwrap_or("unknown"),
    ));
    lines.push(Line::field(
        "Slot",
        &facts
            .slot
            .as_deref()
            .map_or_else(|| "unknown".to_owned(), str::to_uppercase),
    ));
    lines.push(Line::field(
        "Uptime",
        &facts.uptime.map_or_else(|| "unknown".into(), coarse),
    ));
    lines.push(Line::blank());
    lines.push(Line::field("Plex Media Server", facts.plex.word()));
    lines.push(Line::field(
        "Hardware transcoding",
        facts.transcoding.word(),
    ));
    lines.push(Line::blank());

    if facts.addresses.is_empty() {
        lines.push(Line::field("Address", "none"));
    } else {
        for (index, address) in facts.addresses.iter().enumerate() {
            let label = if index == 0 { "Address" } else { "also at" };
            lines.push(Line::field(label, address));
        }
    }
    if let Some(interface) = &facts.interface {
        lines.push(Line::field("Interface", interface));
    }

    // The gate's own words, in full. This is the screen somebody is standing at because
    // the browser is not answering, so summarising here would send them back to the
    // browser they cannot reach.
    if let Verdict::Unhealthy { failures } = &facts.verdict {
        lines.push(Line::blank());
        lines.push(Line::drawn(
            format!("{}Health checks that failed{}", sgr::YELLOW, sgr::RESET),
            25,
        ));
        for failure in failures {
            lines.push(Line::plain(format!("  {failure}")));
        }
    }

    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!("{}ESC  back{}", sgr::DIM, sgr::RESET),
        9,
    ));
    lines
}

/// The word for an action, in the imperative a menu uses.
fn stop_word(choice: Action) -> &'static str {
    match choice {
        Action::Restart => "Restart",
        Action::Off => "Shut down",
    }
}

/// What that action leaves behind, which is the difference somebody is choosing between.
fn stop_outcome(choice: Action) -> &'static str {
    match choice {
        Action::Restart => "The machine goes down and comes back on its own.",
        Action::Off => "The machine goes down and stays down until the power button.",
    }
}

/// One row of a menu: a cursor, or the space where a cursor would be.
///
/// The space matters. A selected row that gains a character the others do not have shifts
/// two columns left every time the cursor moves, and a list that jitters as somebody
/// arrows down it reads as a fault in the screen.
fn menu_row(selected: bool, label: &str) -> Line {
    if selected {
        Line::drawn(
            format!("{}{}>  {label}{}", sgr::BOLD, sgr::CYAN, sgr::RESET),
            label.chars().count() + 3,
        )
    } else {
        Line::plain(format!("   {label}"))
    }
}

/// Choosing between the two ways to stop.
///
/// Both are always listed and the cursor names one, rather than a screen per action reached
/// by its own key. Somebody standing here wants the machine to stop; which of the two they
/// want is the question, and a menu asks it in one place.
fn power(choice: Action, facts: &Facts) -> Vec<Line> {
    let mut lines = vec![
        Line::drawn(format!("{}POWER{}", sgr::BOLD, sgr::RESET), 5),
        Line::blank(),
        Line::blank(),
    ];

    for option in [Action::Restart, Action::Off] {
        lines.push(menu_row(option == choice, stop_word(option)));
    }

    lines.push(Line::blank());
    lines.push(Line::blank());
    // The consequence of whichever row the cursor is on, written out rather than left to
    // the two words above. "Restart" and "Shut down" are not equally obvious to everybody,
    // and this is the last screen before a media server goes dark.
    lines.push(Line::plain(stop_outcome(choice)));

    // Only when there is something to interrupt. A machine with Plex stopped has nothing to
    // warn about, and a warning that is always there is one nobody reads.
    if facts.plex == Plex::Running {
        lines.push(Line::blank());
        lines.push(Line::drawn(
            format!(
                "{}Plex is running. Anything being watched right now will stop.{}",
                sgr::YELLOW,
                sgr::RESET
            ),
            59,
        ));
    }

    lines.push(Line::blank());
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!(
            "{}Up/Down  choose      Enter  continue      ESC  cancel{}",
            sgr::DIM,
            sgr::RESET
        ),
        53,
    ));
    lines
}

/// The one question that has to be answered with a named key.
///
/// Not Enter. Enter is what got somebody here, so a screen that also took Enter would turn
/// two presses of one key into a shutdown — and this screen lives in a room where a person
/// leaning on the desk is a keystroke.
fn power_confirm(choice: Action, facts: &Facts) -> Vec<Line> {
    let question = match choice {
        Action::Restart => "Restart this machine?",
        Action::Off => "Shut this machine down?",
    };

    let mut lines = vec![
        Line::drawn(
            format!("{}{}{question}{}", sgr::BOLD, sgr::YELLOW, sgr::RESET),
            question.chars().count(),
        ),
        Line::blank(),
        Line::blank(),
    ];

    // What the appliance does for them, which is the reason to use this rather than the
    // power button. Somebody who does not know that this stops Plex and flushes the disk
    // has no reason to prefer a menu to five seconds of holding a button.
    lines.push(Line::plain(
        "Plex is stopped cleanly and everything on disk is flushed first.",
    ));
    if facts.plex == Plex::Running {
        lines.push(Line::plain(
            "Anyone watching something right now will be cut off.",
        ));
    }

    lines.push(Line::blank());
    lines.push(Line::blank());
    let press = format!("Press Y to {}", stop_word(choice).to_lowercase());
    lines.push(Line::drawn(
        format!("{}{press}{}", sgr::BOLD, sgr::RESET),
        press.chars().count(),
    ));
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!("{}ESC  no, go back{}", sgr::DIM, sgr::RESET),
        16,
    ));
    lines
}

/// The last frame. Painted before the sequence starts, deliberately.
///
/// `stop_now` does not return, and stopping Plex and remounting `/var` takes seconds — so a
/// screen that started the work first would sit on a menu for the whole of it and then go
/// black, which reads as a machine that ignored the key and then crashed.
fn power_going(choice: Action) -> Vec<Line> {
    let said = match choice {
        Action::Restart => "Restarting...",
        Action::Off => "Shutting down...",
    };
    vec![
        Line::drawn(
            format!("{}{}{said}{}", sgr::BOLD, sgr::CYAN, sgr::RESET),
            said.chars().count(),
        ),
        Line::blank(),
        Line::blank(),
        Line::plain("Stopping Plex and flushing the disk."),
        Line::blank(),
        Line::drawn(
            format!(
                "{}This screen goes blank when it is safe to cut the power.{}",
                sgr::DIM,
                sgr::RESET
            ),
            55,
        ),
    ]
}

fn help() -> Vec<Line> {
    let mut lines = vec![
        Line::drawn(format!("{}HELP{}", sgr::BOLD, sgr::RESET), 4),
        Line::blank(),
    ];
    lines.push(Line::field("P", "Show a QR code that signs a browser in"));
    lines.push(Line::field("D", "Everything this screen knows"));
    lines.push(Line::field("O", "Restart or shut this machine down"));
    lines.push(Line::field("?", "This page"));
    lines.push(Line::field("ESC", "Back to the dashboard"));
    lines.push(Line::blank());
    lines.push(Line::blank());
    lines.push(Line::plain(
        "Everything else is done from a browser on your network.",
    ));
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!(
            "{}A pairing code lasts five minutes and works once. It signs in{}",
            sgr::DIM,
            sgr::RESET
        ),
        60,
    ));
    lines.push(Line::drawn(
        format!(
            "{}the browser that scans it, and nothing else.{}",
            sgr::DIM,
            sgr::RESET
        ),
        44,
    ));
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!(
            "{}A shell is on the second virtual terminal: Alt+F2, Alt+F1 back.{}",
            sgr::DIM,
            sgr::RESET
        ),
        63,
    ));
    lines.push(Line::blank());
    lines.push(Line::drawn(
        format!("{}ESC  back{}", sgr::DIM, sgr::RESET),
        9,
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::super::model::{Plex, Transcoding};
    use super::*;
    use std::time::Duration;

    fn working() -> Facts {
        Facts {
            product: "MediaLith 0.1.0.202608111733".to_owned(),
            version: Some("0.1.0.202608111733".to_owned()),
            slot: Some("b".to_owned()),
            uptime: Some(Duration::from_secs(125)),
            addresses: vec!["192.168.2.102".to_owned()],
            interface: Some("eth0".to_owned()),
            plex: Plex::Running,
            transcoding: Transcoding::Ready,
            verdict: Verdict::Working,
        }
    }

    /// What a person would see, with the colour taken out.
    fn visible(painted: &str) -> String {
        let mut out = String::new();
        let mut chars = painted.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for escaped in chars.by_ref() {
                    if escaped.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn the_power_menu_names_the_outcome_of_the_row_the_cursor_is_on() {
        // Two words are not a choice. "Restart" and "Shut down" are not equally obvious to
        // everybody, and this is the last screen before a media server goes dark -- so the
        // consequence of whichever row is selected is written out underneath it.
        let restart = visible(&frame(
            &Screen::Power {
                choice: Action::Restart,
            },
            &working(),
            101,
            360,
        ));
        assert!(restart.contains("comes back on its own"), "{restart}");
        assert!(
            !restart.contains("stays down"),
            "the other row's outcome is not also shown: {restart}"
        );

        let off = visible(&frame(
            &Screen::Power {
                choice: Action::Off,
            },
            &working(),
            101,
            360,
        ));
        assert!(off.contains("stays down"), "{off}");

        // Both rows are always listed, whichever is selected. A menu that shows only the
        // row under the cursor is not a menu.
        for seen in [&restart, &off] {
            assert!(seen.contains("Restart"), "{seen}");
            assert!(seen.contains("Shut down"), "{seen}");
        }
    }

    #[test]
    fn the_confirmation_asks_for_a_key_that_is_not_the_one_that_opened_it() {
        // Enter is what got somebody to this screen. A confirmation that also took Enter
        // would turn two presses of one key into a shutdown, on a screen that sits in a
        // room where a person leaning on a desk is a keystroke.
        let seen = visible(&frame(
            &Screen::PowerConfirm {
                choice: Action::Off,
            },
            &working(),
            101,
            360,
        ));
        assert!(seen.contains("Press Y to shut down"), "{seen}");
        assert!(
            !seen.contains("Enter"),
            "the screen must not offer the key that brought somebody here: {seen}"
        );
        // And the reason to use this rather than the power button, which is the whole
        // difference between the two and is invisible unless it is said.
        assert!(seen.contains("flushed"), "{seen}");
    }

    #[test]
    fn a_running_plex_is_warned_about_and_a_stopped_one_is_not() {
        // A warning that is always there is one nobody reads. This one is derived from the
        // health check that already runs, so it cannot be true of a machine where it is not.
        let stopped = Facts {
            plex: Plex::Stopped,
            ..working()
        };
        for screen in [
            Screen::Power {
                choice: Action::Off,
            },
            Screen::PowerConfirm {
                choice: Action::Off,
            },
        ] {
            let running = visible(&frame(&screen, &working(), 101, 360));
            let quiet = visible(&frame(&screen, &stopped, 101, 360));
            assert!(
                running.contains("watch"),
                "a running Plex is not warned about in {screen:?}:\n{running}"
            );
            assert!(
                !quiet.contains("watch"),
                "a stopped Plex is warned about anyway in {screen:?}:\n{quiet}"
            );
        }
    }

    #[test]
    fn the_last_frame_says_what_is_happening_and_that_the_screen_will_go_dark() {
        // Painted before the sequence starts. Stopping Plex and flushing a disk takes
        // seconds, and a screen that went black with no explanation is indistinguishable
        // from one that crashed.
        let seen = visible(&frame(
            &Screen::PowerGoing {
                choice: Action::Restart,
            },
            &working(),
            101,
            360,
        ));
        assert!(seen.contains("Restarting"), "{seen}");
        assert!(seen.contains("blank"), "{seen}");
    }

    #[test]
    fn the_cursor_does_not_move_the_rows_it_is_not_on() {
        // A selected row that gains a character the others do not have shifts the list every
        // time the cursor moves, and a menu that jitters as somebody arrows down it reads as
        // a fault in the screen rather than as a selection.
        let selected = menu_row(true, "Restart");
        let not = menu_row(false, "Restart");
        assert_eq!(
            selected.width, not.width,
            "the cursor is drawn in space the row already had"
        );
    }

    #[test]
    fn the_dashboard_says_what_the_machine_is_and_how_to_reach_it() {
        let seen = visible(&frame(&Screen::Dashboard, &working(), 101, 360));
        for expected in [
            "MediaLith",
            "Home Media Appliance",
            "Everything is working",
            "Plex Media Server",
            "Running",
            "Hardware transcoding",
            "Ready",
            "192.168.2.102",
            "https://192.168.2.102",
            "Press P to pair a browser",
            "Slot B",
            "up 2 minutes",
        ] {
            assert!(seen.contains(expected), "missing {expected:?} in:\n{seen}");
        }
    }

    #[test]
    fn a_machine_with_no_address_does_not_offer_a_key_that_would_produce_a_useless_code() {
        // Pressing P with no address would draw a QR code pointing at nothing, which is a
        // worse answer than not offering it. The screen says what to do instead.
        let facts = Facts {
            addresses: Vec::new(),
            interface: None,
            verdict: Verdict::NoNetwork,
            ..working()
        };
        let seen = visible(&frame(&Screen::Dashboard, &facts, 101, 360));

        assert!(seen.contains("No network address"), "{seen}");
        assert!(seen.contains("Waiting for a network address"), "{seen}");
        assert!(seen.contains("cable"), "and says what to do: {seen}");
        assert!(
            !seen.contains("P  Pair a browser"),
            "the key must not be offered: {seen}"
        );
        assert!(seen.contains("D  Details"), "the others still are: {seen}");
    }

    #[test]
    fn every_state_this_appliance_can_be_in_renders_something_a_person_can_act_on() {
        // The five the specification names, plus the two this machine adds. Rendered rather
        // than reasoned about, because a state that panics or draws nothing is a screen
        // somebody meets on the worst day the appliance has.
        let states = [
            (Verdict::Working, "working"),
            (Verdict::NoNetwork, "network"),
            (Verdict::PlexDown, "Plex"),
            (Verdict::NeedsSetup, "set up"),
            (Verdict::OnTrial { tries_left: 2 }, "Testing"),
            (
                Verdict::Recovered {
                    failed: Some("0.1.0.202608120000".to_owned()),
                },
                "Recovered",
            ),
            (
                Verdict::Unhealthy {
                    failures: vec!["var-writable: /var is read-only".to_owned()],
                },
                "read-only",
            ),
        ];

        for (verdict, expected) in states {
            let facts = Facts {
                verdict,
                ..working()
            };
            let seen = visible(&frame(&Screen::Dashboard, &facts, 101, 360));
            assert!(
                seen.contains(expected),
                "a machine in this state says nothing about it: {seen}"
            );
        }
    }

    #[test]
    fn the_pairing_screen_carries_a_symbol_a_countdown_and_the_way_out() {
        let screen = Screen::Pairing {
            url: "https://192.168.2.102/#pair=4K7QM2XR9T8BHVWPQ2M4X6Z8AB".to_owned(),
            seconds_left: 277,
        };
        let painted = frame(&screen, &working(), 101, 360);
        let seen = visible(&painted);

        assert!(seen.contains("PAIR A BROWSER"), "{seen}");
        assert!(seen.contains("Expires in 4:37"), "{seen}");
        assert!(seen.contains("Single use"), "{seen}");
        assert!(seen.contains("ESC  cancel"), "{seen}");
        assert!(
            painted.contains("\x1b[47m") && painted.contains("\x1b[40m"),
            "and the symbol itself, which is drawn in colour rather than in characters"
        );
    }

    #[test]
    fn a_countdown_reads_the_way_every_other_countdown_a_person_has_seen_does() {
        assert_eq!(countdown(300), "5:00");
        assert_eq!(countdown(277), "4:37");
        assert_eq!(countdown(61), "1:01");
        assert_eq!(countdown(9), "0:09");
        assert_eq!(countdown(0), "0:00");
    }

    #[test]
    fn a_pairing_code_appears_on_every_screen_that_has_room_for_one() {
        // The question this was asked in as many words: can it not just fit itself to
        // whatever screen is plugged in? It could not, and the reason was that ten rows
        // were reserved for text before the symbol got any -- so a short screen produced a
        // pairing screen with no pairing code on it.
        //
        // These are the grids the fonts in this image actually produce on panels people
        // own, and every one of them must show a symbol. The text around it is what gives
        // way, in the order it is worth having.
        let screen = Screen::Pairing {
            url: "https://192.168.2.188/#pair=4K7QM2XR9T8BHVWPQ2M4X6Z8AB".to_owned(),
            seconds_left: 277,
        };

        // 1920x1080 at TER16x32 is 33 rows and is deliberately absent: a 37-module symbol
        // cannot be drawn in 33 rows at any whole scale, which is exactly why the font
        // check refuses that font on that panel and takes TER10x18 instead. The screen
        // that says so is covered below.
        for (rows, columns, what) in [
            (50_usize, 180_usize, "2880x1620 at TER16x32"),
            (60, 192, "1920x1080 at TER10x18"),
            (48, 128, "1024x768 at 8x16"),
            (44, 80, "the smallest grid this claims to serve"),
        ] {
            let painted = frame(&screen, &working(), rows, columns);
            assert!(
                painted.contains("\x1b[40m") && painted.contains("\x1b[47m"),
                "no symbol on {what} ({rows}x{columns})"
            );
            let seen = visible(&painted);
            assert!(
                seen.contains("Expires in"),
                "no countdown on {what}: the one part of this screen that changes"
            );
            assert!(seen.contains("ESC  cancel"), "no way out of {what}");
            // And it still does not wrap, which would push everything below it down a row.
            for line in seen.lines() {
                assert!(
                    line.chars().count() <= columns,
                    "{what}: a line of {} columns in {columns}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn a_short_screen_gives_up_its_words_before_it_gives_up_the_code() {
        // What "adapts to the screen" means here, made checkable: the same request drawn
        // on a tall screen and a short one differ by the text, not by the symbol.
        let screen = Screen::Pairing {
            url: "https://192.168.2.188/#pair=4K7QM2XR9T8BHVWPQ2M4X6Z8AB".to_owned(),
            seconds_left: 277,
        };

        let tall = visible(&frame(&screen, &working(), 60, 192));
        let short = visible(&frame(&screen, &working(), 44, 80));

        assert!(tall.contains("PAIR A BROWSER") && tall.contains("Single use"));
        assert!(
            !short.contains("Single use"),
            "a short screen keeps the code and the timer and drops the rest: {short}"
        );
        assert!(short.contains("Expires in"), "{short}");
    }

    #[test]
    fn a_screen_too_small_for_a_symbol_says_so_instead_of_drawing_a_cropped_one() {
        // A cropped QR code looks exactly like a QR code. Somebody would stand there
        // scanning it until they gave up on the feature rather than on the screen.
        let screen = Screen::Pairing {
            url: "https://192.168.2.102/#pair=4K7QM2XR9T8BHVWPQ2M4X6Z8AB".to_owned(),
            seconds_left: 300,
        };
        let seen = visible(&frame(&screen, &working(), 24, 80));
        assert!(seen.contains("too small"), "{seen}");
        assert!(
            seen.contains("recovery device code"),
            "and names the way in that still works: {seen}"
        );
    }

    #[test]
    fn an_expired_code_says_it_expired_rather_than_leaving_a_symbol_on_screen() {
        let seen = visible(&frame(&Screen::PairingExpired, &working(), 101, 360));
        assert!(seen.contains("expired"), "{seen}");
        assert!(seen.contains("Press P"), "{seen}");
        assert!(
            !seen.contains("Expires in"),
            "no countdown for a code that is gone: {seen}"
        );
    }

    #[test]
    fn first_boot_shows_the_recovery_code_and_says_it_will_not_be_shown_again() {
        let screen = Screen::FirstBoot {
            url: Some("https://192.168.2.102/#pair=4K7QM2XR9T8BHVWPQ2M4X6Z8AB".to_owned()),
            recovery_code: "4K7Q-M2XR-9T8B-HVWP".to_owned(),
        };
        let seen = visible(&frame(&screen, &working(), 101, 360));

        assert!(seen.contains("Welcome"), "{seen}");
        assert!(seen.contains("4K7Q-M2XR-9T8B-HVWP"), "{seen}");
        assert!(seen.contains("shown once"), "{seen}");
        assert!(
            seen.contains("Write this down"),
            "the one instruction that matters: {seen}"
        );
    }

    #[test]
    fn first_boot_without_a_network_still_shows_the_recovery_code() {
        // The code exists in a readable form exactly once, and that moment does not wait
        // for a cable. A first-boot screen that showed nothing until the network came up
        // would lose the credential for a machine somebody plugs in tomorrow.
        let screen = Screen::FirstBoot {
            url: None,
            recovery_code: "4K7Q-M2XR-9T8B-HVWP".to_owned(),
        };
        let unplugged = Facts {
            addresses: Vec::new(),
            interface: None,
            verdict: Verdict::NoNetwork,
            ..working()
        };
        let seen = visible(&frame(&screen, &unplugged, 101, 360));
        assert!(seen.contains("4K7Q-M2XR-9T8B-HVWP"), "{seen}");
        assert!(seen.contains("Waiting for a network address"), "{seen}");

        // And the other reason there is no symbol: the machine is reachable and the code
        // simply ran out. Telling somebody to check a cable that is plugged in is the
        // `operstate` mistake this project already recorded, in a friendlier place.
        let seen = visible(&frame(&screen, &working(), 101, 360));
        assert!(seen.contains("expired"), "{seen}");
        assert!(seen.contains("Press P"), "{seen}");
        assert!(!seen.contains("cable"), "{seen}");
    }

    #[test]
    fn help_only_names_keys_that_do_something_and_says_where_the_shell_went() {
        let seen = visible(&frame(&Screen::Help, &working(), 101, 360));
        for expected in ["P", "D", "?", "ESC", "Alt+F2"] {
            assert!(seen.contains(expected), "missing {expected:?}: {seen}");
        }
        assert!(
            !seen.to_lowercase().contains("f12"),
            "no key that does not exist: {seen}"
        );
    }

    #[test]
    fn details_quotes_the_gate_rather_than_summarising_it() {
        // This is the screen somebody is standing at *because* the browser is not
        // answering. Summarising here sends them back to the browser they cannot reach.
        let facts = Facts {
            verdict: Verdict::Unhealthy {
                failures: vec!["usr-verified: dm-verity target is missing".to_owned()],
            },
            ..working()
        };
        let seen = visible(&frame(&Screen::Details, &facts, 101, 360));
        assert!(seen.contains("dm-verity target is missing"), "{seen}");
        assert!(seen.contains("ESC  back"), "{seen}");
    }

    #[test]
    fn the_same_model_paints_the_same_bytes() {
        // What lets the caller not write at all. If this were not true the screen would be
        // repainted several times a second for ever, the kernel's blank timer would never
        // fire, and the panel this machine was asked to let go dark would stay lit.
        let facts = working();
        assert_eq!(
            frame(&Screen::Dashboard, &facts, 101, 360),
            frame(&Screen::Dashboard, &facts, 101, 360)
        );
    }

    #[test]
    fn nothing_is_painted_wider_than_the_screen() {
        // A line longer than the terminal wraps, and one wrapped line pushes everything
        // below it down by a row -- so a single over-long value would misalign the whole
        // screen. The console page had the same defect measured in pixels, found only by
        // measuring the rendered page.
        let long = Facts {
            product: "MediaLith 0.1.0.202608111733-with-a-very-long-suffix".to_owned(),
            addresses: vec!["192.168.200.200".to_owned()],
            interface: Some("enp0s31f6".to_owned()),
            ..working()
        };

        for screen in every_screen() {
            for (rows, columns) in [(101_usize, 360_usize), (25, 80), (43, 132)] {
                for line in visible(&frame(&screen, &long, rows, columns)).lines() {
                    assert!(
                        line.chars().count() <= columns,
                        "{screen:?} at {rows}x{columns} painted {} columns: {line:?}",
                        line.chars().count()
                    );
                }
            }
        }
    }

    /// One of every screen this file can draw.
    ///
    /// The list is written out, and then *checked against the type* by a match the compiler
    /// insists is exhaustive — so adding a variant to [`Screen`] stops this file compiling
    /// until the new screen is in here. A list that enumerates the kinds of a thing is a list
    /// that misses one, and this repository has paid for that in the stylesheet twice; the
    /// difference here is that the miss can be made into a build failure rather than a
    /// property nobody thought to assert.
    fn every_screen() -> Vec<Screen> {
        let all = vec![
            Screen::Dashboard,
            Screen::Pairing {
                url: "https://192.168.2.102/#pair=4K7QM2XR9T8BHVWPQ2M4X6Z8AB".to_owned(),
                seconds_left: 277,
            },
            Screen::PairingExpired,
            Screen::Paired,
            Screen::FirstBoot {
                url: None,
                recovery_code: "4K7Q-M2XR-9T8B-HVWP".to_owned(),
            },
            Screen::Details,
            Screen::Help,
            Screen::Power {
                choice: Action::Restart,
            },
            Screen::PowerConfirm {
                choice: Action::Off,
            },
            Screen::PowerGoing {
                choice: Action::Off,
            },
        ];

        // Nothing is asserted here and nothing needs to be: the value of this loop is that it
        // does not compile when `Screen` grows a variant no arm names.
        for screen in &all {
            match screen {
                Screen::Dashboard
                | Screen::Pairing { .. }
                | Screen::PairingExpired
                | Screen::Paired
                | Screen::FirstBoot { .. }
                | Screen::Details
                | Screen::Help
                | Screen::Power { .. }
                | Screen::PowerConfirm { .. }
                | Screen::PowerGoing { .. } => {}
            }
        }
        all
    }

    #[test]
    fn every_screen_offers_a_way_off_itself() {
        // A screen with nothing to press is a machine somebody has to power-cycle to leave.
        // The two exceptions are the ones that are *about* not going back: the last frame
        // before the machine stops, and the first-boot screen, which says "press any key".
        for screen in every_screen() {
            let seen = visible(&frame(&screen, &working(), 101, 360));
            let has_a_way_out = seen.contains("ESC")
                || seen.contains("Press any key")
                || seen.contains("Press P")
                || seen.contains("Press Y");
            assert!(
                has_a_way_out || matches!(screen, Screen::PowerGoing { .. }),
                "{screen:?} leaves somebody standing at it:\n{seen}"
            );
        }
    }

    #[test]
    fn the_cursor_is_hidden_because_a_block_in_the_middle_of_a_dashboard_looks_like_a_prompt() {
        assert!(frame(&Screen::Dashboard, &working(), 101, 360).contains(sgr::HIDE_CURSOR));
    }
}
