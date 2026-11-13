//! A local copy of a terminal that lives on the Rex server.
//!
//! The server sends a snapshot of the terminal, then everything its program
//! writes. Feeding both to the same engine the server uses (Ghostty's) keeps
//! this copy identical to the server's. The copy never answers the program:
//! the server's terminal does that.

use std::rc::Rc;

use anyhow::{Result, anyhow};
use ghostty_vt::fmt::Format;
use ghostty_vt::render::{CellIterator, CursorVisualStyle, Dirty, RenderState, RowIterator};
use ghostty_vt::screen::{CellWide, Screen, TrackedGridRef};
use ghostty_vt::selection::{FormatOptions, SelectLineOptions, SelectWordOptions, Selection};
use ghostty_vt::snapshot::Decoder;
use ghostty_vt::style::{Palette, PaletteIndex, RgbColor, StyleColor};
pub use ghostty_vt::style::Underline;
use ghostty_vt::terminal::{Options, Point, PointCoordinate, ScrollViewport};
use ghostty_vt::Terminal;
use openrex_proto::{Framed, RexMessage, SnapshotFramer, StreamItem, StreamSplitter};
use serde_json::Value;

use crate::links::{Hyperlink, hyperlink_at, push_hyperlink};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl From<RgbColor> for Rgb {
    fn from(c: RgbColor) -> Self {
        Rgb(c.r, c.g, c.b)
    }
}

/// A run of cells that share a style, drawn as one piece of text.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub col: u16,
    /// Width in cells.
    pub width: u16,
    pub text: String,
    pub fg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: Underline,
    /// The underline's own color, when a program set one (SGR 58).
    pub underline_color: Option<Rgb>,
    pub strike: bool,
    pub overline: bool,
}

impl Span {
    /// Whether the span draws a line through, under, or over its cells, which
    /// spaces carry too.
    pub fn decorated(&self) -> bool {
        self.underline != Underline::None || self.strike || self.overline
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BgSpan {
    pub col: u16,
    pub width: u16,
    pub color: Rgb,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub spans: Vec<Span>,
    pub bgs: Vec<BgSpan>,
    /// Cells a program made into links with OSC 8.
    pub links: Vec<Hyperlink>,
    /// The row's text goes on in the next row: the terminal wrapped it.
    pub wrapped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorShape {
    Block,
    HollowBlock,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorDraw {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgb,
}

/// What to draw for one terminal.
#[derive(Clone, Debug)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub fg: Rgb,
    pub bg: Rgb,
    pub lines: Vec<Rc<Line>>,
    pub cursor: Option<CursorDraw>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MirrorEvent {
    /// The server's terminal changed size.
    Resized { cols: u16, rows: u16 },
    Exited { exit_code: i64 },
}

enum Stage {
    Snapshot(SnapshotFramer),
    Live,
}

pub struct Mirror {
    stage: Stage,
    pub(crate) term: Terminal,
    splitter: StreamSplitter,
    render: RenderState,
    row_iter: RowIterator,
    cell_iter: CellIterator,
    lines: Vec<Rc<Line>>,
    /// Rebuild every line on the next frame.
    stale: bool,
    anchor: Option<TrackedGridRef>,
    has_selection: bool,
    scratch: String,
}

fn err(e: ghostty_vt::Error) -> anyhow::Error {
    anyhow!("terminal engine: {e:?}")
}

impl Mirror {
    pub fn new(nonce: u64) -> Result<Self> {
        crate::graphics::install_png_decoder();
        Ok(Mirror {
            stage: Stage::Snapshot(SnapshotFramer::new()),
            term: Terminal::new(Options::default()).map_err(err)?,
            splitter: StreamSplitter::new(nonce),
            render: RenderState::new().map_err(err)?,
            row_iter: RowIterator::new().map_err(err)?,
            cell_iter: CellIterator::new().map_err(err)?,
            lines: Vec::new(),
            stale: true,
            anchor: None,
            has_selection: false,
            scratch: String::new(),
        })
    }

    /// True once the snapshot has arrived and the copy shows real content.
    pub fn is_live(&self) -> bool {
        matches!(self.stage, Stage::Live)
    }

    /// The engine's own terminal, for what the copy holds beyond text, such
    /// as Kitty graphics images.
    pub(crate) fn terminal(&self) -> &Terminal {
        &self.term
    }

    /// Feeds stream bytes, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<MirrorEvent> {
        let mut events = Vec::new();
        match &mut self.stage {
            Stage::Live => self.feed_live(bytes, &mut events),
            Stage::Snapshot(framer) => match framer.push(bytes) {
                Framed::Pending => {}
                Framed::NotASnapshot(all) => {
                    self.stage = Stage::Live;
                    self.feed_live(&all, &mut events);
                }
                Framed::Complete { snapshot, rest } => {
                    self.stage = Stage::Live;
                    match Decoder::from_slice(&snapshot).decode() {
                        Ok(term) => {
                            // The anchor belongs to the terminal it replaces.
                            self.anchor = None;
                            self.has_selection = false;
                            self.term = term;
                            self.stale = true;
                            let (cols, rows) = self.size();
                            events.push(MirrorEvent::Resized { cols, rows });
                        }
                        Err(e) => log::error!("could not decode the terminal snapshot: {e:?}"),
                    }
                    self.feed_live(&rest, &mut events);
                }
            },
        }
        events
    }

    fn feed_live(&mut self, bytes: &[u8], events: &mut Vec<MirrorEvent>) {
        for item in self.splitter.push(bytes) {
            match item {
                StreamItem::Vt(data) => self.term.vt_write(&data),
                StreamItem::Rex(RexMessage::Resize { cols, rows, cell_width_px, cell_height_px, .. }) => {
                    if cols > 0 && rows > 0 {
                        if let Err(e) =
                            self.term.resize(cols, rows, cell_width_px.max(1), cell_height_px.max(1))
                        {
                            log::error!("resizing the terminal copy: {e:?}");
                        }
                        self.stale = true;
                        events.push(MirrorEvent::Resized { cols, rows });
                    }
                }
                StreamItem::Rex(RexMessage::Theme(theme)) => self.apply_theme(&theme),
                StreamItem::Rex(RexMessage::ChildExited { exit_code }) => {
                    events.push(MirrorEvent::Exited { exit_code });
                }
                StreamItem::Rex(RexMessage::Unknown(_)) => {}
            }
        }
        // The copy's effects (replies to the program, bells, titles) are the
        // server's to act on. Drop them so they don't pile up.
        self.term.events().for_each(drop);
    }

    fn apply_theme(&mut self, theme: &Value) {
        let color = |key: &str| theme[key].as_str().and_then(parse_hex).map(RgbColor::from);
        self.term.set_default_fg_color(color("foreground")).ok();
        self.term.set_default_bg_color(color("background")).ok();
        self.term.set_default_cursor_color(color("cursor")).ok();
        let mut palette = Palette::default();
        let mut changed = false;
        for entry in theme["palette"].as_array().into_iter().flatten() {
            let Some((index, hex)) = entry.as_str().and_then(|s| s.split_once('=')) else {
                continue;
            };
            if let (Ok(index), Some(rgb)) = (index.trim().parse::<u8>(), parse_hex(hex)) {
                palette.set(PaletteIndex(index.into()), rgb.into());
                changed = true;
            }
        }
        self.term.set_default_color_palette(changed.then_some(palette)).ok();
        self.stale = true;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.cols().unwrap_or(80), self.term.rows().unwrap_or(24))
    }

    pub fn is_mouse_tracking(&self) -> bool {
        self.term.is_mouse_tracking().unwrap_or(false)
    }

    pub fn is_alt_screen(&self) -> bool {
        matches!(self.term.active_screen(), Ok(Screen::Alternate))
    }

    /// Scrolls the view into the scrollback. Negative is up.
    pub fn scroll(&mut self, rows: isize) {
        if rows != 0 {
            self.term.scroll_viewport(ScrollViewport::Delta(rows));
            self.stale = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        if !self.term.is_viewport_active().unwrap_or(true) {
            self.term.scroll_viewport(ScrollViewport::Bottom);
            self.stale = true;
        }
    }

    fn clamp(&self, col: i32, row: i32) -> Point {
        let (cols, rows) = self.size();
        Point::Viewport(PointCoordinate {
            x: col.clamp(0, cols as i32 - 1) as u16,
            y: row.clamp(0, rows as i32 - 1) as u32,
        })
    }

    /// Starts a selection at a cell of the view.
    pub fn select_start(&mut self, col: i32, row: i32) {
        self.clear_selection();
        self.anchor = self.term.track_grid_ref(self.clamp(col, row)).ok();
    }

    /// Extends the selection from where it started to a cell of the view.
    pub fn select_to(&mut self, col: i32, row: i32) {
        let Some(anchor) = &self.anchor else { return };
        let Ok(Some(start)) = anchor.snapshot(&self.term) else { return };
        let Ok(end) = self.term.grid_ref(self.clamp(col, row)) else { return };
        let selection = Selection::new(start, end, false);
        if self.term.set_selection(Some(&selection)).is_ok() {
            self.has_selection = true;
            self.stale = true;
        }
    }

    /// Selects the word at a cell of the view.
    pub fn select_word(&mut self, col: i32, row: i32) {
        self.clear_selection();
        let Ok(at) = self.term.grid_ref(self.clamp(col, row)) else { return };
        let Ok(Some(selection)) = self.term.select_word(SelectWordOptions::new(at)) else { return };
        if self.term.set_selection(Some(&selection)).is_ok() {
            self.has_selection = true;
            self.stale = true;
        }
    }

    /// Selects the line at a cell of the view, following soft wraps.
    pub fn select_line(&mut self, col: i32, row: i32) {
        self.clear_selection();
        let Ok(at) = self.term.grid_ref(self.clamp(col, row)) else { return };
        let Ok(Some(selection)) = self.term.select_line(SelectLineOptions::new(at)) else { return };
        if self.term.set_selection(Some(&selection)).is_ok() {
            self.has_selection = true;
            self.stale = true;
        }
    }

    /// Selects everything, scrollback included.
    pub fn select_all(&mut self) {
        self.clear_selection();
        let (cols, _) = self.size();
        let total = self.term.total_rows().unwrap_or(0);
        if total == 0 {
            return;
        }
        let at = |x: u16, y: u32| Point::Screen(PointCoordinate { x, y });
        let (Ok(start), Ok(end)) = (self.term.grid_ref(at(0, 0)), self.term.grid_ref(at(cols.saturating_sub(1), total as u32 - 1))) else {
            return;
        };
        let selection = Selection::new(start, end, false);
        if self.term.set_selection(Some(&selection)).is_ok() {
            self.has_selection = true;
            self.stale = true;
        }
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
        if self.has_selection {
            self.term.set_selection(None).ok();
            self.has_selection = false;
            self.stale = true;
        }
    }

    pub fn has_selection(&self) -> bool {
        self.has_selection
    }

    /// The selected text, as a terminal copies it: soft-wrapped lines joined
    /// and trailing blanks trimmed.
    pub fn selection_text(&self) -> Option<String> {
        if !self.has_selection {
            return None;
        }
        let options = FormatOptions::new()
            .with_emit_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true);
        let bytes = self.term.format_selection_alloc(options).ok()??;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (!text.is_empty()).then_some(text)
    }

    /// Reads what to draw. Lines that didn't change since the last call are
    /// shared with it.
    pub fn frame(&mut self) -> Result<Frame> {
        let snapshot = self.render.update(&self.term).map_err(err)?;
        let colors = snapshot.colors().map_err(err)?;
        let default_fg: Rgb = colors.foreground.into();
        let default_bg: Rgb = colors.background.into();
        let cols = snapshot.cols().map_err(err)?;
        let rows = snapshot.rows().map_err(err)?;

        let dirty = snapshot.dirty().map_err(err)?;
        let full = self.stale || dirty == Dirty::Full || self.lines.len() != rows as usize;
        if full {
            self.lines.clear();
            self.lines.resize(rows as usize, Rc::new(Line::default()));
        }
        if full || dirty != Dirty::Clean {
            let (sel_fg, sel_bg) = (default_bg, default_fg);
            let mut iter = self.row_iter.update(&snapshot).map_err(err)?;
            let mut y = 0usize;
            while let Some(row) = iter.next() {
                if y >= self.lines.len() {
                    break;
                }
                if full || row.dirty().unwrap_or(true) {
                    let mut cells = self.cell_iter.update(row).map_err(err)?;
                    let mut line = Line::default();
                    let raw_row = row.raw_row().ok();
                    line.wrapped = raw_row.and_then(|r| r.is_wrapped().ok()).unwrap_or(false);
                    let has_links = raw_row.and_then(|r| r.has_hyperlink().ok()).unwrap_or(false);
                    let mut x: u16 = 0;
                    while let Some(cell) = cells.next() {
                        let col = x;
                        x += 1;
                        let wide = cell.raw_cell().and_then(|c| c.wide()).unwrap_or(CellWide::Narrow);
                        if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                            continue;
                        }
                        let width: u16 = if wide == CellWide::Wide { 2 } else { 1 };
                        let linked = has_links && cell.raw_cell().and_then(|c| c.has_hyperlink()).unwrap_or(false);
                        if let Some(uri) = linked.then(|| hyperlink_at(&self.term, col, y as u32)).flatten() {
                            push_hyperlink(&mut line.links, col, width, uri);
                        }

                        let style = if cell.has_styling().unwrap_or(false) {
                            cell.style().unwrap_or_default()
                        } else {
                            Default::default()
                        };
                        let mut fg: Rgb =
                            cell.fg_color().ok().flatten().map(Into::into).unwrap_or(default_fg);
                        let mut bg: Option<Rgb> = cell.bg_color().ok().flatten().map(Into::into);
                        if style.inverse {
                            let old_fg = fg;
                            fg = bg.unwrap_or(default_bg);
                            bg = Some(old_fg);
                        }
                        if cell.is_selected().unwrap_or(false) {
                            fg = sel_fg;
                            bg = Some(sel_bg);
                        }
                        if let Some(color) = bg.filter(|c| *c != default_bg) {
                            match line.bgs.last_mut() {
                                Some(last) if last.color == color && last.col + last.width == col => {
                                    last.width += width;
                                }
                                _ => line.bgs.push(BgSpan { col, width, color }),
                            }
                        }

                        self.scratch.clear();
                        if !style.invisible {
                            cell.graphemes_utf8(&mut self.scratch).ok();
                        }
                        let decorated =
                            style.underline != Underline::None || style.strikethrough || style.overline;
                        if self.scratch.is_empty() {
                            if !decorated {
                                continue;
                            }
                            self.scratch.push(' ');
                        }
                        let ascii = self.scratch.len() == 1 && self.scratch.is_ascii();
                        if self.scratch == " " && !decorated {
                            // A plain space only matters inside a run of text,
                            // and must not extend a run's underline or strike.
                            if let Some(last) = line.spans.last_mut() {
                                if last.col + last.width == col && last.text.is_ascii() && !last.decorated() {
                                    last.text.push(' ');
                                    last.width += 1;
                                }
                            }
                            continue;
                        }
                        let span = Span {
                            col,
                            width,
                            text: String::new(),
                            fg,
                            bold: style.bold,
                            italic: style.italic,
                            faint: style.faint,
                            underline: style.underline,
                            underline_color: match style.underline_color {
                                StyleColor::Rgb(c) => Some(c.into()),
                                StyleColor::Palette(i) => Some(colors.palette[i.0 as usize].into()),
                                _ => None,
                            },
                            strike: style.strikethrough,
                            overline: style.overline,
                        };
                        match line.spans.last_mut() {
                            // Only plain ASCII is batched: its glyphs are one
                            // cell wide, so the run stays on the grid.
                            Some(last)
                                if ascii
                                    && last.text.is_ascii()
                                    && last.col + last.width == col
                                    && same_style(last, &span) =>
                            {
                                last.text.push_str(&self.scratch);
                                last.width += width;
                            }
                            _ => {
                                line.spans.push(Span { text: self.scratch.clone(), ..span });
                            }
                        }
                    }
                    for span in &mut line.spans {
                        let trimmed = span.text.trim_end_matches(' ').len();
                        if trimmed > 0 && trimmed < span.text.len() && !span.decorated() {
                            span.width -= (span.text.len() - trimmed) as u16;
                            span.text.truncate(trimmed);
                        }
                    }
                    if *self.lines[y] != line {
                        self.lines[y] = Rc::new(line);
                    }
                }
                y += 1;
            }
        }

        let cursor = snapshot.cursor().map_err(err)?;
        let cursor = match cursor.viewport {
            Some(at) if cursor.visible => Some(CursorDraw {
                col: at.x.saturating_sub(at.at_wide_tail as u16),
                row: at.y,
                shape: match cursor.visual_style {
                    CursorVisualStyle::Bar => CursorShape::Bar,
                    CursorVisualStyle::Underline => CursorShape::Underline,
                    CursorVisualStyle::BlockHollow => CursorShape::HollowBlock,
                    _ => CursorShape::Block,
                },
                color: colors.cursor.map(Into::into).unwrap_or(default_fg),
            }),
            _ => None,
        };
        snapshot.clean().map_err(err)?;
        self.stale = false;

        Ok(Frame { cols, rows, fg: default_fg, bg: default_bg, lines: self.lines.clone(), cursor })
    }
}

fn same_style(a: &Span, b: &Span) -> bool {
    a.fg == b.fg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.faint == b.faint
        && a.underline == b.underline
        && a.underline_color == b.underline_color
        && a.strike == b.strike
        && a.overline == b.overline
}

impl From<Rgb> for RgbColor {
    fn from(c: Rgb) -> Self {
        RgbColor { r: c.0, g: c.1, b: c.2 }
    }
}

/// Parses `RRGGBB` or `#RRGGBB`.
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.is_ascii() {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(frame: &Frame, row: usize) -> String {
        let mut out = String::new();
        for span in &frame.lines[row].spans {
            while (out.chars().count() as u16) < span.col {
                out.push(' ');
            }
            out.push_str(&span.text);
        }
        out
    }

    fn live(nonce: u64) -> Mirror {
        let mut m = Mirror::new(nonce).unwrap();
        // Output that doesn't start with a snapshot goes straight to the copy.
        m.feed(b"\x1b[H");
        assert!(m.is_live());
        m
    }

    #[test]
    fn draws_text_with_styles_and_backgrounds() {
        let mut m = live(1);
        m.feed(b"plain \x1b[1;31mbold red\x1b[0m \x1b[44mon blue\x1b[0m\r\nsecond");
        let f = m.frame().unwrap();
        assert_eq!(text_of(&f, 0), "plain bold red on blue");
        assert_eq!(text_of(&f, 1), "second");
        let line = &f.lines[0];
        let bold = line.spans.iter().find(|s| s.text.starts_with("bold")).unwrap();
        assert!(bold.bold);
        assert_ne!(bold.fg, f.fg);
        assert_eq!(line.bgs.len(), 1);
        assert_eq!((line.bgs[0].col, line.bgs[0].width), (15, 7));
        assert_eq!(f.cursor.map(|c| (c.col, c.row)), Some((6, 1)));
    }

    #[test]
    fn decorations_keep_their_style_and_stop_at_their_text() {
        let mut m = live(1);
        m.feed(b"\x1b[4mund\x1b[0m \x1b[4:3;58;5;196mcurl\x1b[0m \x1b[9mst\x1b[0m \x1b[53mov\x1b[0m x");
        let f = m.frame().unwrap();
        let spans = &f.lines[0].spans;
        let find = |t: &str| spans.iter().find(|s| s.text.starts_with(t)).unwrap();
        // The plain space after each decorated run stays out of it.
        assert_eq!((find("und").text.as_str(), find("und").width), ("und", 3));
        assert_eq!(find("und").underline, Underline::Single);
        assert_eq!(find("curl").text, "curl");
        assert_eq!(find("curl").underline, Underline::Curly);
        assert_eq!(find("curl").underline_color, Some(Rgb(0xff, 0, 0)));
        assert_eq!(find("und").underline_color, None);
        assert_eq!((find("st").text.as_str(), find("st").strike), ("st", true));
        assert_eq!((find("ov").text.as_str(), find("ov").overline), ("ov", true));
        assert_eq!(text_of(&f, 0), "und curl st ov x");
    }

    #[test]
    fn wide_characters_get_their_own_span_and_two_cells() {
        let mut m = live(1);
        m.feed("ab\u{4e2d}cd".as_bytes());
        let f = m.frame().unwrap();
        let spans = &f.lines[0].spans;
        assert_eq!(spans.len(), 3);
        assert_eq!((spans[1].text.as_str(), spans[1].col, spans[1].width), ("\u{4e2d}", 2, 2));
        assert_eq!((spans[2].text.as_str(), spans[2].col), ("cd", 4));
    }

    #[test]
    fn follows_the_servers_size_and_theme() {
        let mut m = live(9);
        let events = m.feed(b"\x1b_rex1;9;{\"type\":\"resize\",\"cols\":100,\"rows\":30}\x1b\\");
        assert_eq!(events, vec![MirrorEvent::Resized { cols: 100, rows: 30 }]);
        assert_eq!(m.size(), (100, 30));
        m.feed(b"\x1b_rex1;9;{\"type\":\"theme\",\"foreground\":\"#102030\",\"background\":\"0a0b0c\",\"palette\":[\"1=ff0000\"]}\x1b\\");
        m.feed(b"\x1b[31mx");
        let f = m.frame().unwrap();
        assert_eq!((f.cols, f.rows), (100, 30));
        assert_eq!(f.fg, Rgb(0x10, 0x20, 0x30));
        assert_eq!(f.bg, Rgb(0x0a, 0x0b, 0x0c));
        assert_eq!(f.lines[0].spans[0].fg, Rgb(0xff, 0, 0));
        let events = m.feed(b"\x1b_rex1;9;{\"type\":\"child_exited\",\"exit_code\":3}\x1b\\");
        assert_eq!(events, vec![MirrorEvent::Exited { exit_code: 3 }]);
    }

    #[test]
    fn selects_and_copies_text() {
        let mut m = live(1);
        m.feed(b"hello world\r\nnext line");
        assert_eq!(m.selection_text(), None);
        m.select_start(6, 0);
        m.select_to(3, 1);
        assert_eq!(m.selection_text().as_deref(), Some("world\nnext"));
        let f = m.frame().unwrap();
        // Selected cells are drawn inverted.
        assert!(f.lines[0].bgs.iter().any(|b| b.col == 6 && b.color == f.fg));
        m.clear_selection();
        assert_eq!(m.selection_text(), None);
        assert!(m.frame().unwrap().lines[0].bgs.is_empty());
    }

    #[test]
    fn selects_words_and_lines() {
        let mut m = live(1);
        m.feed(b"alpha beta-gamma delta\r\nsecond");
        m.select_word(7, 0);
        let word = m.selection_text().unwrap();
        assert!(word.starts_with("beta"), "{word}");
        assert!(!word.contains(' '), "{word}");
        m.select_line(3, 0);
        assert_eq!(m.selection_text().as_deref(), Some("alpha beta-gamma delta"));
    }

    #[test]
    fn unchanged_lines_are_shared_between_frames() {
        let mut m = live(1);
        m.feed(b"one\r\ntwo");
        let a = m.frame().unwrap();
        m.feed(b"!");
        let b = m.frame().unwrap();
        assert!(Rc::ptr_eq(&a.lines[0], &b.lines[0]));
        assert_eq!(text_of(&b, 1), "two!");
    }

    #[test]
    fn scrollback_scrolls_and_returns() {
        let mut m = live(1);
        for i in 0..60 {
            m.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(text_of(&m.frame().unwrap(), 0), "line 37");
        m.scroll(-10);
        assert_eq!(text_of(&m.frame().unwrap(), 0), "line 27");
        m.scroll_to_bottom();
        assert_eq!(text_of(&m.frame().unwrap(), 0), "line 37");
    }
}
