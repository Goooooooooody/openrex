//! Find in a terminal's screen and scrollback: client.find.open, client.find.next, client.find.previous, client.find.close.
//!
//! The searching is Ghostty's own (its `search` module), run on the pane's
//! local copy of the terminal: it covers the scrollback, follows the
//! terminal as output arrives, keeps track of the chosen match as lines
//! scroll away, and scrolls the view to show it. Ghostty compares ASCII
//! letters without regard to case, so smart case is done here: a query with
//! an uppercase letter keeps only the matches whose text has its exact case.
//!
//! "Next" goes up, toward older output, as in Ghostty: a search usually
//! starts from the prompt at the bottom. Matches are counted newest first,
//! so the count climbs as Find Next goes up.

use gpui::{
    App, BorderStyle, Bounds, Context, Div, Entity, Hsla, InteractiveElement, KeyDownEvent, ParentElement,
    Pixels, SharedString, Styled, Window, div, fill, outline, point, px, rgb, size,
};
use ghostty_vt::Terminal;
use ghostty_vt::fmt::Format;
use ghostty_vt::screen::{CellWide, GridRef};
use ghostty_vt::search::{Search, Status};
use ghostty_vt::selection::{FormatOptions, Selection};
use ghostty_vt::terminal::{PointCoordinate, PointSpace};
use serde_json::{Value, json};

use crate::actions::{Outcome, Target};
use crate::palette;
use crate::pane::Pane;
use crate::term::Mirror;
use crate::workspace::Workspace;

/// How many times one refresh feeds the search before letting a frame be
/// drawn. A huge scrollback is searched over several frames.
const FEED_ROUNDS: usize = 64;

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    args: &Value,
    _target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    let older = match action {
        "client.find.open" => {
            let query = args.get("query").and_then(Value::as_str).map(str::to_string);
            return Some(open(ws, query, cx).map(|count| count.to_json()));
        }
        "client.find.close" => {
            close(ws, cx);
            return Some(Ok(Value::Null));
        }
        "client.find.next" => true,
        "client.find.previous" => false,
        _ => return None,
    };
    Some(step(ws, older, cx).map(|count| count.to_json()))
}

/// The find bar: whether it is showing and what it looks for.
#[derive(Debug, Default)]
pub struct FindBar {
    pub(crate) open: bool,
    query: String,
    /// The block being searched: the focused one, followed as focus moves.
    block: Option<String>,
    /// What was last looked for, so Find Next works with the bar closed.
    last: String,
}

/// Shows the find bar over the focused terminal. A query replaces what the
/// bar had; without one, the bar keeps it.
fn open(ws: &mut Workspace, query: Option<String>, cx: &mut Context<Workspace>) -> Result<Count, (String, String)> {
    if ws.store.read(cx).focused_pane().is_none() {
        return Err(("conflict".into(), "no pane is focused".into()));
    }
    if !ws.find.open {
        ws.find.open = true;
        ws.find.query.clear();
    }
    if let Some(query) = query {
        ws.find.query = query;
    }
    Ok(apply(ws, cx))
}

/// Hides the find bar and its highlights. The search stays with its pane,
/// so Find Next carries on from the same match, until find is used in
/// another pane.
fn close(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    ws.find.open = false;
    if let Some(pane) = ws.find.block.as_ref().and_then(|id| ws.store.read(cx).panes.get(id).cloned()) {
        pane.update(cx, |pane, cx| {
            if let Some(finder) = &mut pane.find {
                finder.shown = false;
            }
            cx.notify();
        });
    }
}

/// Moves to the next (older) or previous (newer) match, opening the bar
/// with the last query when it was closed.
fn step(ws: &mut Workspace, older: bool, cx: &mut Context<Workspace>) -> Result<Count, (String, String)> {
    let Some(pane) = target(ws, cx) else { return Err(("conflict".into(), "no pane is focused".into())) };
    ws.find.open = true;
    // A closed bar, or one opened afresh and still empty, looks again for
    // what was last looked for.
    if ws.find.query.is_empty() {
        ws.find.query = ws.find.last.clone();
    }
    let query = ws.find.query.clone();
    if query.is_empty() {
        return Ok(Count::default());
    }
    Ok(with_finder(&pane, true, cx, |finder, mirror| {
        finder.toward(mirror, &query, older);
        finder.count
    })
    .unwrap_or_default())
}

/// Points the search at the focused pane and gives it the bar's query.
fn apply(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Count {
    if !ws.find.query.is_empty() {
        ws.find.last = ws.find.query.clone();
    }
    let query = ws.find.query.clone();
    let Some(pane) = target(ws, cx) else { return Count::default() };
    with_finder(&pane, true, cx, |finder, mirror| {
        finder.set_needle(mirror, &query);
        finder.count
    })
    .unwrap_or_default()
}

/// The focused pane, which the bar follows: the search leaves the pane that
/// lost focus.
fn target(ws: &mut Workspace, cx: &mut App) -> Option<Entity<Pane>> {
    let pane = ws.store.read(cx).focused_pane()?;
    let id = pane.read(cx).block_id.clone();
    if ws.find.block.as_ref() != Some(&id) {
        if let Some(old) = ws.find.block.take().and_then(|id| ws.store.read(cx).panes.get(&id).cloned()) {
            old.update(cx, |old, cx| {
                old.find = None;
                cx.notify();
            });
        }
        ws.find.block = Some(id);
    }
    Some(pane)
}

/// Runs `f` on a pane's search, made when it has none, once the pane's
/// terminal has arrived. `notify` redraws the pane, which a caller in the
/// middle of drawing must not ask for.
fn with_finder<R>(
    pane: &Entity<Pane>,
    notify: bool,
    cx: &mut App,
    f: impl FnOnce(&mut Finder, &mut Mirror) -> R,
) -> Option<R> {
    pane.update(cx, |pane, cx| {
        let mirror = pane.mirror.as_mut().filter(|m| m.is_live())?;
        let finder = pane.find.get_or_insert_with(Finder::default);
        finder.shown = true;
        let out = f(finder, mirror);
        if notify {
            cx.notify();
        }
        Some(out)
    })
}

/// Keeps the open bar's search on the focused pane and caught up with its
/// terminal. Called as the window is drawn, so the count shown is current.
pub(crate) fn update(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !ws.find.open {
        return;
    }
    let Some(pane) = target(ws, cx) else {
        // Nothing to search: no session, or no pane in it.
        ws.find.open = false;
        ws.find.block = None;
        return;
    };
    let query = ws.find.query.clone();
    let busy = with_finder(&pane, false, cx, |finder, mirror| {
        finder.set_needle(mirror, &query);
        finder.refresh(mirror)
    });
    if busy == Some(true) {
        window.request_animation_frame();
    }
}

/// Handles a key while the find bar is open. False passes it on to the key
/// bindings: keys held with Ctrl, Alt, or Super that the bar has no use
/// for, so shortcuts keep working.
pub(crate) fn on_key(ws: &mut Workspace, ev: &KeyDownEvent, cx: &mut Context<Workspace>) -> bool {
    let ks = &ev.keystroke;
    let m = &ks.modifiers;
    let mut query = ws.find.query.clone();
    match ks.key.as_str() {
        "escape" => {
            close(ws, cx);
            return true;
        }
        "enter" => {
            step(ws, !m.shift, cx).ok();
            return true;
        }
        "up" if !m.control && !m.alt => {
            step(ws, true, cx).ok();
            return true;
        }
        "down" if !m.control && !m.alt => {
            step(ws, false, cx).ok();
            return true;
        }
        "backspace" if m.control || m.alt => palette::delete_word(&mut query),
        "backspace" => {
            query.pop();
        }
        "w" if m.control && !m.shift => palette::delete_word(&mut query),
        "u" if m.control && !m.shift => query.clear(),
        "v" if m.control => {
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                query.push_str(text.lines().next().unwrap_or(""));
            }
        }
        _ if m.control || m.alt || m.platform => return false,
        _ => {
            if let Some(text) = ks.key_char.as_ref().filter(|t| !t.chars().any(char::is_control)) {
                query.push_str(text);
            }
        }
    }
    if query != ws.find.query {
        ws.find.query = query;
        apply(ws, cx);
    }
    true
}

/// How many matches there are and which one is chosen, counted newest
/// first from 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Count {
    pub current: Option<usize>,
    pub total: usize,
    /// False while the search is still going through the scrollback.
    pub done: bool,
}

impl Count {
    fn to_json(self) -> Value {
        json!({"current": self.current, "total": self.total})
    }
}

/// What the bar shows for a count.
fn count_label(query: &str, count: Count) -> String {
    match count {
        _ if query.is_empty() => String::new(),
        Count { total: 0, done: false, .. } => "…".into(),
        Count { total: 0, .. } => "No matches".into(),
        Count { current: Some(i), total, .. } => format!("{i}/{total}"),
        Count { current: None, total, .. } => format!("{total} found"),
    }
}

/// Smart case: a query with an uppercase letter is matched case-sensitively.
fn case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

/// The position, counted from 1, of the chosen match among the ones that
/// count, given which of all matches count, in the same order.
fn rank(counted: &[bool], selected: Option<usize>) -> Option<usize> {
    let i = selected?;
    counted.get(i).copied().filter(|c| *c)?;
    Some(counted[..i].iter().filter(|c| **c).count() + 1)
}

/// A run of highlighted cells in one row of the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    /// The chosen match, drawn more strongly.
    pub current: bool,
}

/// The runs of cells a match covers in the view, from its first cell to its
/// last (inclusive). A missing end is outside the view: a missing start is
/// above it, a missing end below.
fn segments(start: Option<PointCoordinate>, end: Option<PointCoordinate>, cols: u16, rows: u16) -> Vec<(u16, u16, u16)> {
    if (start.is_none() && end.is_none()) || cols == 0 || rows == 0 {
        return Vec::new();
    }
    let (r0, c0) = start.map_or((0, 0), |p| (p.y, p.x));
    let (r1, c1) = end.map_or((rows as u32 - 1, cols - 1), |p| (p.y, p.x));
    let mut out = Vec::new();
    for row in r0..=r1.min(rows as u32 - 1) {
        let from = if row == r0 { c0 } else { 0 };
        let to = if row == r1 { c1 } else { cols - 1 }.min(cols - 1);
        if to >= from {
            out.push((row as u16, from, to - from + 1));
        }
    }
    out
}

/// One terminal's search, which the find bar drives.
#[derive(Default)]
pub struct Finder {
    search: Option<Search>,
    needle: String,
    case_sensitive: bool,
    /// Choose a match as soon as the search has one: true for the newest
    /// (as Find Next does from nothing), false for the oldest.
    jump: Option<bool>,
    /// For a case-sensitive needle, which of the engine's matches (newest
    /// first) have its exact case, and the engine's total they were worked
    /// out for.
    exact: Option<(usize, Vec<bool>)>,
    pub count: Count,
    /// False while the bar is closed: nothing is highlighted.
    pub shown: bool,
}

impl Finder {
    /// Looks for `needle`, from scratch when it changed, and moves to its
    /// first match. An empty needle stops looking. True when it changed.
    pub fn set_needle(&mut self, mirror: &mut Mirror, needle: &str) -> bool {
        let changed = self.change_needle(mirror, needle, true);
        if changed {
            self.refresh(mirror);
        }
        changed
    }

    /// Moves to the next match in a direction, as [`Finder::step`] does,
    /// first looking for `needle` if it is new: then the move is to its
    /// first match that way.
    pub fn toward(&mut self, mirror: &mut Mirror, needle: &str, older: bool) {
        if self.change_needle(mirror, needle, older) {
            self.refresh(mirror);
        } else {
            self.step(mirror, older);
        }
    }

    fn change_needle(&mut self, mirror: &Mirror, needle: &str, older: bool) -> bool {
        if needle == self.needle {
            return false;
        }
        self.needle = needle.to_string();
        self.case_sensitive = case_sensitive(needle);
        self.exact = None;
        self.jump = (!needle.is_empty()).then_some(older);
        if let Some(search) = &mut self.search {
            let needle = (!needle.is_empty()).then_some(needle);
            if search.set_needle(mirror.terminal(), needle).is_err() {
                self.search = None;
            }
        }
        true
    }

    /// Moves to the next older match, or the next newer one, wrapping
    /// around, and scrolls the view to show it.
    pub fn step(&mut self, mirror: &mut Mirror, older: bool) {
        // A first match still to be chosen is this move: once the refresh
        // chooses it, moving again would skip one.
        let pending = self.jump.is_some();
        if pending {
            self.jump = Some(older);
        }
        self.refresh(mirror);
        if !pending && self.select(mirror.terminal(), older) {
            mirror.redraw();
        }
        self.recount(mirror.terminal());
    }

    /// Catches up with the terminal: new output, lines that scrolled away,
    /// or a terminal replaced by a new snapshot. True while there is more
    /// of the scrollback to search.
    pub fn refresh(&mut self, mirror: &mut Mirror) -> bool {
        if self.needle.is_empty() {
            self.count = Count { done: true, ..Count::default() };
            return false;
        }
        let done = match self.catch_up(mirror.terminal()) {
            Some(done) => done,
            None => {
                // The search's terminal is gone, replaced by a snapshot or a
                // new stream. Start again on the one there is now.
                self.search = None;
                self.exact = None;
                self.catch_up(mirror.terminal()).unwrap_or(true)
            }
        };
        if let Some(older) = self.jump {
            let found = self.search.as_ref().and_then(|s| s.total_matches().ok()).unwrap_or(0) > 0;
            if found {
                self.ensure_exact(mirror.terminal());
                if self.select(mirror.terminal(), older) {
                    mirror.redraw();
                    self.jump = None;
                }
            }
            if done {
                self.jump = None;
            }
        }
        self.recount(mirror.terminal());
        self.count.done = done;
        !done
    }

    /// Feeds the search until it is caught up, or for a while. `None` when
    /// the search can't be used.
    fn catch_up(&mut self, term: &Terminal) -> Option<bool> {
        if self.search.is_none() {
            let mut search = Search::new(term).ok()?;
            search.set_needle(term, Some(self.needle.as_str())).ok()?;
            self.search = Some(search);
        }
        let search = self.search.as_mut()?;
        for _ in 0..FEED_ROUNDS {
            search.feed(term).ok()?;
            while search.tick().ok()? == Status::Running {}
            if search.status().ok()? == Status::Complete {
                return Some(true);
            }
        }
        Some(false)
    }

    /// Chooses the next match in a direction, skipping those whose case
    /// doesn't fit. False when there is none.
    fn select(&mut self, term: &Terminal, older: bool) -> bool {
        let Some(search) = &mut self.search else { return false };
        if self.case_sensitive && self.exact.as_ref().is_some_and(|(_, e)| !e.contains(&true)) {
            return false;
        }
        let total = search.total_matches().unwrap_or(0);
        for _ in 0..total.max(1) {
            let moved = if older { search.select_next(term) } else { search.select_prev(term) };
            if !moved.unwrap_or(false) {
                return false;
            }
            if !self.case_sensitive {
                return true;
            }
            if search.selected_match(term).ok().flatten().is_some_and(|m| has_case(term, &m, &self.needle)) {
                return true;
            }
        }
        false
    }

    /// Works out which matches have the needle's exact case, once per
    /// change in the number of matches.
    fn ensure_exact(&mut self, term: &Terminal) {
        let Some(search) = &self.search else { return };
        if !self.case_sensitive {
            return;
        }
        let total = search.total_matches().unwrap_or(0);
        if self.exact.as_ref().is_some_and(|(t, _)| *t == total) {
            return;
        }
        let flags = search
            .matches(term)
            .unwrap_or_default()
            .iter()
            .map(|m| has_case(term, m, &self.needle))
            .collect();
        self.exact = Some((total, flags));
    }

    fn recount(&mut self, term: &Terminal) {
        self.ensure_exact(term);
        let Some(search) = &self.search else {
            self.count = Count { done: self.count.done, ..Count::default() };
            return;
        };
        let total = search.total_matches().unwrap_or(0);
        let selected = search.selected_index().ok().flatten();
        let (current, total) = match &self.exact {
            Some((_, flags)) if self.case_sensitive => (rank(flags, selected), flags.iter().filter(|f| **f).count()),
            _ => (selected.filter(|i| *i < total).map(|i| i + 1), total),
        };
        self.count = Count { current, total, done: self.count.done };
    }

    /// The matches in view, as runs of cells to highlight.
    pub fn marks(&self, mirror: &Mirror) -> Vec<Mark> {
        let Some(search) = self.search.as_ref().filter(|_| self.shown && !self.needle.is_empty()) else {
            return Vec::new();
        };
        let term = mirror.terminal();
        let (cols, rows) = mirror.size();
        let mut out = Vec::new();
        let add = |m: &Selection, current: bool, out: &mut Vec<Mark>| {
            let at = |g: &GridRef<'_>| term.point_from_grid_ref(g, PointSpace::Viewport).ok().flatten();
            let (start, mut end) = (at(&m.start()), at(&m.end()));
            if start.is_none() && end.is_none() {
                return;
            }
            if self.case_sensitive && !has_case(term, m, &self.needle) {
                return;
            }
            // A wide character's second cell belongs to it too.
            if let Some(e) = &mut end
                && m.end().cell().and_then(|c| c.wide()).ok() == Some(CellWide::Wide)
            {
                e.x = (e.x + 1).min(cols.saturating_sub(1));
            }
            out.extend(segments(start, end, cols, rows).into_iter().map(|(row, col, width)| Mark { row, col, width, current }));
        };
        for m in search.viewport_matches(term).unwrap_or_default() {
            add(&m, false, &mut out);
        }
        if let Ok(Some(m)) = search.selected_match(term) {
            add(&m, true, &mut out);
        }
        out
    }
}

/// Whether a match's text has the needle's exact case. The engine ignores
/// the case of ASCII letters.
fn has_case(term: &Terminal, m: &Selection, needle: &str) -> bool {
    let options = FormatOptions::new().with_emit_format(Format::Plain).with_unwrap(true).with_selection(m);
    match term.format_selection_alloc(options) {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).contains(needle),
        _ => false,
    }
}

/// A pane's highlights for this frame. The search is fed first: the
/// matches it holds point into the terminal as of the last feed, and lines
/// that have since left the scrollback, or a terminal replaced by a new
/// snapshot, would leave them pointing at freed memory. Feeding may also
/// scroll the view to a first match, so call this before reading the frame.
pub(crate) fn marks(pane: &mut Pane, window: &Window) -> Vec<Mark> {
    let (Some(finder), Some(mirror)) = (&mut pane.find, &mut pane.mirror) else { return Vec::new() };
    if !finder.shown || !mirror.is_live() {
        return Vec::new();
    }
    if finder.refresh(mirror) {
        window.request_animation_frame();
    }
    finder.marks(mirror)
}

fn match_color() -> Hsla {
    let mut c: Hsla = rgb(0xe0af68).into();
    c.a = 0.35;
    c
}

fn current_color() -> Hsla {
    rgb(0xff9e64).into()
}

/// Paints the highlights of one row of the grid, over the cells'
/// backgrounds and under their text.
pub(crate) fn paint_row(
    marks: &[Mark],
    row: usize,
    left: Pixels,
    cell_w: Pixels,
    top: Pixels,
    line_h: Pixels,
    window: &mut Window,
) {
    for mark in marks.iter().filter(|m| m.row as usize == row) {
        let x0 = (left + cell_w * mark.col as f32).floor();
        let x1 = (left + cell_w * (mark.col + mark.width) as f32).ceil();
        let cell = Bounds::new(point(x0, top), size(x1 - x0, line_h));
        if mark.current {
            let mut strong = current_color();
            strong.a = 0.55;
            window.paint_quad(fill(cell, strong));
            window.paint_quad(outline(cell, current_color(), BorderStyle::Solid));
        } else {
            window.paint_quad(fill(cell, match_color()));
        }
    }
}

/// The find bar, over the top right of the pane it searches.
pub(crate) fn render_bar(bar: &FindBar, block_id: &str, pane: &Entity<Pane>, cx: &App) -> Option<Div> {
    if !bar.open || bar.block.as_deref() != Some(block_id) {
        return None;
    }
    let (text, dim, accent, panel, red): (Hsla, Hsla, Hsla, Hsla, Hsla) =
        (rgb(0xc0caf5).into(), rgb(0x6b7394).into(), rgb(0x7aa2f7).into(), rgb(0x1a1b26).into(), rgb(0xf7768e).into());
    let count = pane.read(cx).find.as_ref().map(|f| f.count).unwrap_or_default();
    let label = count_label(&bar.query, count);
    let missing = count.done && count.total == 0 && !bar.query.is_empty();
    let (query, query_color) = if bar.query.is_empty() { ("Find".to_string(), dim) } else { (bar.query.clone(), text) };
    let field = div()
        .flex()
        .items_center()
        .min_w(px(120.0))
        .max_w(px(320.0))
        .overflow_hidden()
        .text_color(query_color)
        .child(div().min_w_0().truncate().child(SharedString::from(query)))
        .child(div().flex_shrink_0().w(px(2.0)).h(px(14.0)).ml(px(1.0)).bg(accent));
    let mut bar_el = div()
        .absolute()
        .top(px(6.0))
        .right(px(10.0))
        .occlude()
        .flex()
        .items_center()
        .gap_3()
        .px_2()
        .py_1()
        .bg(panel)
        .border_1()
        .border_color(if missing { red } else { accent })
        .rounded_md()
        .shadow_md()
        .text_size(px(12.0))
        .child(field)
        .child(div().flex_shrink_0().text_color(if missing { red } else { text }).child(SharedString::from(label)));
    if case_sensitive(&bar.query) {
        bar_el = bar_el.child(div().flex_shrink_0().text_color(dim).child("Aa"));
    }
    Some(bar_el.child(div().flex_shrink_0().text_color(dim).text_size(px(11.0)).child("⏎ next  ⇧⏎ prev  esc")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: u16, y: u32) -> Option<PointCoordinate> {
        Some(PointCoordinate { x, y })
    }

    fn live(text: &str) -> Mirror {
        let mut m = Mirror::new(1).unwrap();
        m.feed(b"\x1b[H");
        m.feed(text.as_bytes());
        m
    }

    #[test]
    fn smart_case() {
        assert!(!case_sensitive("foo bar"));
        assert!(case_sensitive("Foo"));
        assert!(!case_sensitive("123 -_"));
    }

    #[test]
    fn labels_counts() {
        let c = |current, total, done| Count { current, total, done };
        assert_eq!(count_label("", c(None, 0, true)), "");
        assert_eq!(count_label("x", c(None, 0, false)), "…");
        assert_eq!(count_label("x", c(None, 0, true)), "No matches");
        assert_eq!(count_label("x", c(Some(3), 17, true)), "3/17");
        assert_eq!(count_label("x", c(None, 4, true)), "4 found");
    }

    #[test]
    fn ranks_among_counted_matches() {
        let counted = [false, true, false, true, true];
        assert_eq!(rank(&counted, Some(1)), Some(1));
        assert_eq!(rank(&counted, Some(4)), Some(3));
        assert_eq!(rank(&counted, Some(2)), None);
        assert_eq!(rank(&counted, Some(9)), None);
        assert_eq!(rank(&counted, None), None);
    }

    #[test]
    fn splits_matches_into_rows() {
        // On one row.
        assert_eq!(segments(at(5, 2), at(7, 2), 10, 4), vec![(2, 5, 3)]);
        // Wrapped onto the next row.
        assert_eq!(segments(at(8, 0), at(1, 1), 10, 4), vec![(0, 8, 2), (1, 0, 2)]);
        // Starting above the view.
        assert_eq!(segments(None, at(2, 0), 10, 4), vec![(0, 0, 3)]);
        // Ending below it.
        assert_eq!(segments(at(9, 3), None, 10, 4), vec![(3, 9, 1)]);
        // Not in view at all.
        assert!(segments(None, None, 10, 4).is_empty());
    }

    #[test]
    fn steps_through_many_matches_in_order() {
        let text: String = (1..=40).map(|i| format!("line {i} needle\r\n")).collect();
        let mut m = Mirror::new(1).unwrap();
        m.feed(b"\x1b[H");
        m.feed(b"\x1b_rex1;1;{\"type\":\"resize\",\"cols\":80,\"rows\":90}\x1b\\");
        m.feed(text.as_bytes());
        let mut f = Finder { shown: true, ..Finder::default() };
        assert!(f.set_needle(&mut m, "needle"));
        let row = |f: &Finder, m: &Mirror| f.marks(m).into_iter().find(|k| k.current).map(|k| k.row);
        assert_eq!(f.count.current, Some(1));
        assert_eq!(row(&f, &m), Some(39));
        f.step(&mut m, true);
        assert_eq!((f.count.current, row(&f, &m)), (Some(2), Some(38)));
        f.step(&mut m, true);
        assert_eq!((f.count.current, row(&f, &m)), (Some(3), Some(37)));
    }

    #[test]
    fn counts_and_steps_through_matches_newest_first() {
        let mut m = live("one foo\r\ntwo foo\r\nthree FOO\r\n");
        let mut f = Finder { shown: true, ..Finder::default() };
        assert!(f.set_needle(&mut m, "foo"));
        // Lowercase finds every case, and starts at the newest.
        assert_eq!(f.count, Count { current: Some(1), total: 3, done: true });
        let current = |f: &Finder, m: &Mirror| f.marks(m).into_iter().find(|k| k.current);
        assert_eq!(current(&f, &m), Some(Mark { row: 2, col: 6, width: 3, current: true }));
        f.step(&mut m, true);
        assert_eq!(f.count.current, Some(2));
        assert_eq!(current(&f, &m).map(|k| k.row), Some(1));
        f.step(&mut m, true);
        f.step(&mut m, true);
        // Wraps around past the oldest.
        assert_eq!(f.count.current, Some(1));
        f.step(&mut m, false);
        assert_eq!(f.count.current, Some(3));
        assert_eq!(f.marks(&m).iter().filter(|k| !k.current).count(), 3);
    }

    #[test]
    fn a_new_needle_moves_once_to_its_first_match_either_way() {
        let mut m = live("foo 1\r\nfoo 2\r\nfoo 3\r\n");
        let mut f = Finder { shown: true, ..Finder::default() };
        f.toward(&mut m, "foo", true);
        assert_eq!(f.count.current, Some(1));
        f.toward(&mut m, "foo", true);
        assert_eq!(f.count.current, Some(2));

        let mut g = Finder { shown: true, ..Finder::default() };
        g.toward(&mut m, "foo", false);
        assert_eq!(g.count.current, Some(3));
        g.toward(&mut m, "foo", false);
        assert_eq!(g.count.current, Some(2));
    }

    #[test]
    fn uppercase_matches_case_sensitively() {
        let mut m = live("Foo foo\r\nfoo FOO Foo\r\n");
        let mut f = Finder { shown: true, ..Finder::default() };
        f.set_needle(&mut m, "Foo");
        assert_eq!(f.count, Count { current: Some(1), total: 2, done: true });
        let marks = f.marks(&m);
        let others: Vec<_> = marks.iter().filter(|k| !k.current).map(|k| (k.row, k.col)).collect();
        assert_eq!(others.len(), 2);
        assert!(others.contains(&(0, 0)) && others.contains(&(1, 8)), "{others:?}");
        assert_eq!(marks.iter().find(|k| k.current).map(|k| (k.row, k.col)), Some((1, 8)));
        f.step(&mut m, true);
        assert_eq!(f.count.current, Some(2));
        assert_eq!(f.marks(&m).iter().find(|k| k.current).map(|k| (k.row, k.col)), Some((0, 0)));
        f.step(&mut m, true);
        assert_eq!(f.count.current, Some(1));

        f.set_needle(&mut m, "FOo");
        assert_eq!(f.count, Count { current: None, total: 0, done: true });
        assert!(f.marks(&m).is_empty());
    }

    #[test]
    fn scrolls_to_matches_in_the_scrollback_and_follows_output() {
        let mut m = live("needle up here\r\n");
        for i in 0..60 {
            m.feed(format!("line {i}\r\n").as_bytes());
        }
        let mut f = Finder { shown: true, ..Finder::default() };
        f.set_needle(&mut m, "needle");
        assert_eq!(f.count.total, 1);
        // The view scrolled up to show it.
        assert_eq!(f.marks(&m).iter().find(|k| k.current).map(|k| (k.row, k.col)), Some((0, 0)));

        m.scroll_to_bottom();
        m.feed(b"another needle\r\n");
        f.refresh(&mut m);
        assert_eq!(f.count, Count { current: Some(2), total: 2, done: true });
        assert_eq!(f.marks(&m).iter().filter(|k| !k.current).count(), 1);
        f.step(&mut m, false);
        assert_eq!(f.count.current, Some(1));
    }

    #[test]
    fn hidden_or_empty_searches_draw_nothing() {
        let mut m = live("abc abc");
        let mut f = Finder::default();
        f.set_needle(&mut m, "abc");
        assert_eq!(f.count.total, 2);
        assert!(f.marks(&m).is_empty());
        f.shown = true;
        assert_eq!(f.marks(&m).len(), 3);
        f.set_needle(&mut m, "");
        assert_eq!(f.count, Count { current: None, total: 0, done: true });
        assert!(f.marks(&m).is_empty());
    }

    #[test]
    fn starts_again_on_a_replaced_terminal() {
        let mut m = live("hello there");
        let mut f = Finder { shown: true, ..Finder::default() };
        f.set_needle(&mut m, "there");
        assert_eq!(f.count.total, 1);
        // A new stream brings a new copy of the terminal.
        m = live("there and there");
        f.refresh(&mut m);
        assert_eq!(f.count.total, 2);
    }

    #[test]
    fn highlights_both_cells_of_a_wide_character() {
        let mut m = live("ab\u{4e2d}cd");
        let mut f = Finder { shown: true, ..Finder::default() };
        f.set_needle(&mut m, "b\u{4e2d}");
        let current = f.marks(&m).into_iter().find(|k| k.current);
        assert_eq!(current.map(|k| (k.col, k.width)), Some((1, 3)));
    }
}
