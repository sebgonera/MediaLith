//! Drawing a QR code on a text console (ADR-0019).
//!
//! The encoding is [`qrcode`]'s. The *rendering* is here, because a terminal has cells and
//! not pixels and the two questions have nothing to do with each other — and because
//! rendering is the half that decides whether a phone can read the result. The spec that
//! matters for the encoder is ISO/IEC 18004; the spec that matters here is a camera.
//!
//! # Two cells wide, one row tall
//!
//! A console character is twice as tall as it is wide. On the reference laptop the panel
//! is 2880x1620 with an 8x16 font, so a module drawn one cell square would be 8 px across
//! and 16 px down — a QR code stretched to twice its height. Decoders find the finder
//! patterns by their 1:1:3:1:1 ratio *in both axes*, and a symbol at 1:2 is one many of
//! them refuse rather than one they read slowly.
//!
//! So a module is two cells wide and one row tall, which on this hardware is 16 px by
//! 16 px: square. That is the same lesson the console page's sparkline learned from the
//! other end — `preserveAspectRatio="none"` turned a round end marker into a smear — and
//! it is worth stating in both places, because in neither case does anything except a
//! rendered picture show it.
//!
//! # No Unicode, no colour dependency
//!
//! Every module is a run of spaces with a background colour: black for dark, white for
//! light. Nothing here needs a glyph, so nothing here depends on which font the kernel
//! console happens to have loaded — and this image's command line asks for `TER16x32` and
//! gets 8x16, which is exactly the kind of surprise a design should not be resting on.
//!
//! The half-block characters (`▀`) would halve the height needed and were rejected for
//! that reason: they need the console font to carry U+2580, which the built-in kernel fonts
//! may or may not do, and the failure mode is a screen full of blank cells that looks like
//! a broken feature rather than a missing glyph.
//!
//! # The quiet zone is not optional
//!
//! Four modules of light on every side, which the standard requires and which decoders
//! genuinely depend on. Drawn explicitly rather than left to the surrounding screen being
//! dark, because the surrounding screen *is* dark and the quiet zone has to be light.

use qrcode::{Color, EcLevel, QrCode};

/// Modules of light around the symbol, per ISO/IEC 18004.
pub const QUIET: usize = 4;

/// Cells across for one module. See the module documentation on aspect ratio.
pub const CELLS_PER_MODULE: usize = 2;

/// A symbol, ready to be drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Modules per side, quiet zone excluded.
    width: usize,
    /// Row-major, `true` for dark.
    dark: Vec<bool>,
}

impl Symbol {
    /// Encodes `payload`.
    ///
    /// # Errors
    /// If the payload is too long for any version, which for the URLs this draws cannot
    /// happen — but a `Result` rather than a panic, because the alternative to reporting it
    /// is a dashboard that takes the appliance down when somebody gives it a long hostname.
    pub fn encode(payload: &str) -> Result<Self, String> {
        // Error correction L, deliberately, where the crate's own default is M.
        //
        // Error correction buys tolerance of a damaged symbol, and this one is not printed
        // on a box that gets scuffed -- it is drawn on a monitor a metre from the camera and
        // it lasts five minutes. What it costs is size: the same payload needs a version 4
        // symbol at M and a version 3 at L, which is 33 modules against 29.
        //
        // Four modules is the difference between fitting and not. The console runs at
        // 180x50 once the Terminus font is compiled in, and after the wordmark, the
        // countdown and the footer there are forty rows for the symbol. At M there is no
        // whole-number scale that fits, and the screen would have to say it had no room --
        // on the very machine whose font was made bigger to help.
        //
        // Fewer modules in the same space also means *larger* modules, which is the thing a
        // camera actually cares about.
        let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)
            .map_err(|error| format!("this address will not fit in a QR code: {error}"))?;

        Ok(Self {
            width: code.width(),
            dark: code
                .to_colors()
                .into_iter()
                .map(|colour| colour == Color::Dark)
                .collect(),
        })
    }

    /// Modules per side, quiet zone excluded.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Modules per side including the quiet zone, which is what has to fit on a screen.
    #[must_use]
    pub fn drawn_width(&self) -> usize {
        self.width + QUIET * 2
    }

    /// Whether the module at `(row, column)` of the symbol is dark.
    ///
    /// Symbol coordinates: `(0, 0)` is the corner of the finder pattern, not the corner of
    /// the drawing. Outside the symbol is light.
    #[must_use]
    pub fn is_dark(&self, row: usize, column: usize) -> bool {
        if row >= self.width || column >= self.width {
            return false;
        }
        self.dark
            .get(row * self.width + column)
            .copied()
            .unwrap_or(false)
    }

    /// Whether the cell at `(row, column)` **of the drawing** is dark.
    ///
    /// Drawing coordinates include the quiet zone, so `(0, 0)` is its outer corner. This is
    /// what makes the quiet zone fall out of one lookup instead of being a special case in
    /// the drawing loop — and `checked_sub` rather than a subtraction is what keeps that
    /// true: inside the quiet zone the subtraction has no answer, which is exactly the
    /// question being asked.
    #[must_use]
    pub fn drawn_is_dark(&self, row: usize, column: usize) -> bool {
        let (Some(row), Some(column)) = (row.checked_sub(QUIET), column.checked_sub(QUIET)) else {
            return false;
        };
        self.is_dark(row, column)
    }

    /// The largest whole-number scale that fits in `rows` by `columns` cells.
    ///
    /// Whole numbers only. A module drawn 2.5 cells wide is one drawn 2 cells wide half the
    /// time and 3 the other half, and a QR code whose modules are not all the same size is
    /// one a decoder has to guess at — which it does by giving up.
    ///
    /// `None` when even the smallest scale does not fit. That is a real answer and the
    /// caller has to have one: drawing a symbol with the edges off the screen produces
    /// something that looks like a QR code and is not one, which is worse than a sentence
    /// saying the screen is too small.
    #[must_use]
    pub fn scale_for(&self, rows: usize, columns: usize) -> Option<usize> {
        let modules = self.drawn_width();
        let by_width = columns / (modules * CELLS_PER_MODULE);
        let by_height = rows / modules;
        let scale = by_width.min(by_height);
        (scale >= 1).then_some(scale)
    }

    /// The symbol as terminal rows, at `scale`.
    ///
    /// Each returned string is one screen row, already carrying the colour changes. They
    /// are not positioned: the caller decides where on the screen this goes, because the
    /// caller is the only thing that knows what else is on it.
    #[must_use]
    pub fn rows(&self, scale: usize) -> Vec<String> {
        let modules = self.drawn_width();
        let mut out = Vec::with_capacity(modules * scale);

        for module_row in 0..modules {
            let mut line = String::with_capacity(modules * CELLS_PER_MODULE * scale + 16);
            let mut current: Option<bool> = None;

            for module_column in 0..modules {
                let dark = self.drawn_is_dark(module_row, module_column);
                // The colour is written only where it changes. A symbol is mostly runs, so
                // this is roughly a third of the bytes -- which matters because every one
                // of them crosses a virtual terminal that is also a serial console.
                if current != Some(dark) {
                    line.push_str(if dark { DARK } else { LIGHT });
                    current = Some(dark);
                }
                for _ in 0..CELLS_PER_MODULE * scale {
                    line.push(' ');
                }
            }
            line.push_str(RESET);

            // Repeated rather than recomputed: the row is identical `scale` times over, and
            // building it once is the difference between a screen write and a screen write
            // with a nested loop in front of it.
            for _ in 0..scale {
                out.push(line.clone());
            }
        }

        out
    }
}

/// Background black.
const DARK: &str = "\x1b[40m";
/// Background white. Colour 7 rather than bright white: 40–47 are the eight backgrounds
/// every terminal has had since the beginning, and the brighter pair is an extension. A
/// decoder thresholds locally, so light grey against black is contrast enough.
const LIGHT: &str = "\x1b[47m";
/// Back to whatever the screen was using.
const RESET: &str = "\x1b[0m";

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload the appliance would actually produce.
    const PAYLOAD: &str = "https://192.168.2.102/#pair=4K7QM2XR9T8BHVWPQ2M4X6Z8AB";

    #[test]
    fn a_payload_encodes_to_a_square_symbol() {
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        assert!(symbol.width() >= 21, "version 1 is the smallest there is");
        assert_eq!(symbol.dark.len(), symbol.width() * symbol.width());
    }

    #[test]
    fn the_finder_patterns_are_where_the_standard_puts_them() {
        // Pinned against ISO/IEC 18004 rather than against this module's own output, which
        // would agree with itself however wrong it was. Every QR code has a 7x7 finder in
        // three corners, and the middle of each is a solid 3x3 of dark surrounded by a ring
        // of light. If the rows came back transposed or the colours inverted -- the two
        // mistakes a renderer actually makes -- this is what would catch it.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        let far = symbol.width() - 7;

        for (top, left) in [(0_usize, 0_usize), (0, far), (far, 0)] {
            for row in 0..7 {
                for column in 0..7 {
                    let ring = row == 0 || row == 6 || column == 0 || column == 6;
                    let middle = (2..=4).contains(&row) && (2..=4).contains(&column);
                    let expected = ring || middle;
                    assert_eq!(
                        symbol.is_dark(top + row, left + column),
                        expected,
                        "finder at ({top},{left}), module ({row},{column})"
                    );
                }
            }
        }
    }

    #[test]
    fn everything_outside_the_symbol_is_light_so_the_quiet_zone_needs_no_special_case() {
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        // Read in drawing coordinates, which is where the quiet zone exists: every cell in
        // the outer four rings, on all four sides.
        let drawn = symbol.drawn_width();
        for ring in 0..QUIET {
            for along in 0..drawn {
                assert!(!symbol.drawn_is_dark(ring, along), "top ring {ring}");
                assert!(!symbol.drawn_is_dark(drawn - 1 - ring, along), "bottom");
                assert!(!symbol.drawn_is_dark(along, ring), "left");
                assert!(!symbol.drawn_is_dark(along, drawn - 1 - ring), "right");
            }
        }
    }

    #[test]
    fn the_drawn_symbol_carries_four_modules_of_quiet_on_every_side() {
        // Not optional, and not something the surrounding screen can provide: the screen
        // around this is dark and the quiet zone has to be light.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        assert_eq!(symbol.drawn_width(), symbol.width() + 8);

        let rows = symbol.rows(1);
        assert_eq!(rows.len(), symbol.drawn_width());

        for edge in [&rows[0], &rows[QUIET - 1], rows.last().unwrap()] {
            assert!(
                !edge.contains(DARK),
                "a row inside the quiet zone must have no dark module at all"
            );
        }
    }

    #[test]
    fn a_module_is_twice_as_wide_as_it_is_tall_because_a_console_cell_is_not_square() {
        // The failure this prevents is a symbol at 1:2, which decoders find by the finder
        // pattern's 1:1:3:1:1 ratio in both axes and mostly refuse rather than read slowly.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        let rows = symbol.rows(1);

        let cells = visible_cells(&rows[0]);
        assert_eq!(
            cells,
            symbol.drawn_width() * CELLS_PER_MODULE,
            "each module is {CELLS_PER_MODULE} cells across"
        );
        assert_eq!(rows.len(), symbol.drawn_width(), "and one row down");
    }

    #[test]
    fn scaling_multiplies_both_axes_together() {
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        for scale in 1..=3 {
            let rows = symbol.rows(scale);
            assert_eq!(rows.len(), symbol.drawn_width() * scale);
            assert_eq!(
                visible_cells(&rows[0]),
                symbol.drawn_width() * CELLS_PER_MODULE * scale
            );
        }
    }

    #[test]
    fn the_reference_laptops_console_gets_a_symbol_big_enough_to_scan() {
        // 2880x1620 with an 8x16 font, measured on the machine rather than taken from the
        // kernel command line -- which asks for 1280x720 and TER16x32 and gets neither,
        // because i915 drives the panel at its native mode once it takes the console over.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        let scale = symbol
            .scale_for(101, 360)
            .expect("this console has room for a pairing code");
        assert!(scale >= 2, "a scale of {scale} would be a small symbol");

        // And it really fits, which is the assertion that catches an off-by-one in
        // scale_for rather than in the reasoning about it.
        let rows = symbol.rows(scale);
        assert!(rows.len() <= 101, "{} rows", rows.len());
        assert!(visible_cells(&rows[0]) <= 360);
    }

    #[test]
    fn the_symbol_still_fits_once_the_console_font_gets_bigger() {
        // The screen this is drawn on has half as many rows as it used to. CONFIG_FONTS was
        // unset, so the Terminus fonts were dropped by kconfig and the panel ran at 8x16 --
        // 360x101. With them compiled in it runs at 16x32, which is 180x50, and a symbol
        // laid out for a hundred rows would report that it had no room on the very machine
        // the change was made to help.
        //
        // The physical size does not change, which is the part worth understanding: the
        // modules are drawn in cells and the cells are twice as big, so the symbol covers
        // the same number of pixels either way.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        let available = 50 - 12;
        let scale = symbol
            .scale_for(available, 180)
            .expect("a 180x50 console has room for a pairing code");

        let rows = symbol.rows(scale);
        assert!(
            rows.len() <= available,
            "{} rows of the {available} available",
            rows.len()
        );
        assert!(visible_cells(&rows[0]) <= 180);
    }

    #[test]
    fn a_screen_with_no_room_says_so_rather_than_drawing_something_cropped() {
        // A cropped QR code looks exactly like a QR code and is not one. The caller has to
        // be able to tell, so it gets None rather than a scale of zero.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        assert_eq!(symbol.scale_for(24, 80), None);
        assert_eq!(symbol.scale_for(0, 0), None);
    }

    #[test]
    fn what_reaches_the_screen_is_the_symbol_and_not_its_transpose() {
        // The mistake `rows` can make that nothing else here would catch: swapping the two
        // indices. A transposed QR code is still a plausible-looking QR code -- and it
        // would still have three finder patterns, because the test above reads them through
        // `is_dark` rather than through what is drawn.
        //
        // A symbol is not symmetric: three corners carry a finder and the fourth does not,
        // so reading the drawn cells back and comparing them against the symbol catches a
        // swap, an inversion and an off-by-one in the quiet zone at once.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        let scale = 2;
        let rows = symbol.rows(scale);

        for (screen_row, line) in rows.iter().enumerate() {
            let cells = drawn_cells(line);
            for (screen_column, dark) in cells.iter().enumerate() {
                let module_row = screen_row / scale;
                let module_column = screen_column / (CELLS_PER_MODULE * scale);
                assert_eq!(
                    *dark,
                    symbol.drawn_is_dark(module_row, module_column),
                    "cell ({screen_row},{screen_column}) belongs to module \
                     ({module_row},{module_column})"
                );
            }
        }
    }

    /// One boolean per cell the terminal would paint: `true` where the background is black.
    fn drawn_cells(row: &str) -> Vec<bool> {
        let mut out = Vec::new();
        let mut dark = false;
        let mut chars = row.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(dark);
                continue;
            }
            let mut sequence = String::from(c);
            for escaped in chars.by_ref() {
                sequence.push(escaped);
                if escaped == 'm' {
                    break;
                }
            }
            if sequence == DARK {
                dark = true;
            } else if sequence == LIGHT {
                dark = false;
            }
        }
        out
    }

    #[test]
    fn every_row_ends_by_giving_the_screen_back() {
        // A row that left the terminal on a white background would paint the rest of the
        // screen with it, and the symptom is a dashboard that turns into a white block
        // below the QR code.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        for row in symbol.rows(2) {
            assert!(row.ends_with(RESET), "{row:?}");
        }
    }

    #[test]
    fn nothing_drawn_needs_a_glyph_the_console_font_might_not_have() {
        // The reason this is spaces and colour rather than half-blocks: a missing glyph
        // renders as a blank cell, so the failure looks like a broken feature rather than
        // like a font problem, and nothing in this repository could see it.
        let symbol = Symbol::encode(PAYLOAD).expect("encodes");
        for row in symbol.rows(2) {
            let drawn: String = strip_escapes(&row);
            assert!(
                drawn.chars().all(|c| c == ' '),
                "every drawn cell must be a space: {drawn:?}"
            );
        }
    }

    /// The characters a terminal would actually put on the screen.
    fn strip_escapes(row: &str) -> String {
        let mut out = String::new();
        let mut chars = row.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for escaped in chars.by_ref() {
                    if escaped == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn visible_cells(row: &str) -> usize {
        strip_escapes(row).chars().count()
    }
}
