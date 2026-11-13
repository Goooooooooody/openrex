//! Typing beyond plain keys: input methods and dead keys, and jumping between shell prompts (client.prompt.previous, client.prompt.next).
//!
//! A plain key press goes to the terminal from `Pane::on_key_down`, which
//! describes the physical key as well as its text, and stops the event there.
//! Text that doesn't come from one key press arrives through gpui's input
//! handler instead: what an input method (ibus, fcitx) commits, a dead key
//! that didn't combine with the next key, and the composition still in
//! progress, which is drawn at the cursor but not sent until it's committed.
//! Because the key press is stopped in `on_key_down`, gpui never hands the
//! same key to the input handler, so nothing is typed twice. A key in the
//! middle of a compose sequence types nothing and isn't sent at all.
//!
//! Committed text goes to the server as a `client.key` with an unidentified
//! key and the text, not as a paste: the server types it as it is, and a
//! program with bracketed paste on doesn't see it bracketed.

use std::ops::Range;

use ghostty_vt::screen::RowSemanticPrompt;
use ghostty_vt::terminal::{Point as GridPoint, PointCoordinate};
use gpui::{
    App, Bounds, ContentMask, Context, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Font, Hsla,
    IntoElement, Keystroke, Pixels, Point, SharedString, Styled, TextRun, UTF16Selection, UnderlineStyle, Window,
    canvas, fill, point, px, size,
};
use serde_json::{Value, json};

use crate::actions::{Outcome, Target};
use crate::pane::{Metrics, Pane, hsla};
use crate::term::{Frame, Mirror};
use crate::workspace::Workspace;

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    _args: &Value,
    target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    let forward = match action {
        "client.prompt.previous" => false,
        "client.prompt.next" => true,
        _ => return None,
    };
    let pane = {
        let store = ws.store.read(cx);
        match &target.block_id {
            Some(block) => store.panes.get(block).cloned(),
            None => store.focused_pane(),
        }
    };
    // With no prompt in that direction there's nowhere to go, which isn't a
    // failure worth a message.
    if let Some(pane) = pane {
        pane.update(cx, |pane, cx| pane.jump_to_prompt(forward, cx));
    }
    Some(Ok(Value::Null))
}

/// The `client.key` key for text that no single key press typed, such as
/// what an input method commits. The server types `text` as it is, without
/// bracketing it as a paste, in every keyboard mode a program can ask for.
pub fn text_key(text: &str) -> Value {
    json!({
        "key": "unidentified",
        "mods": "",
        "consumed_mods": "",
        "text": text,
        "unshifted_codepoint": 0,
        "composing": false,
        "action": "press",
    })
}

/// A pane's state for input methods.
#[derive(Debug, Default)]
pub(crate) struct Ime {
    /// Text an input method or dead key is still composing. It's drawn at
    /// the cursor, and typed only once it's committed.
    marked: String,
    /// The terminal cursor's cell, where it was last drawn, in window
    /// coordinates. Input methods open their candidate window next to it.
    cursor: Option<Bounds<Pixels>>,
    /// The cursor moved since the platform was last told where it is.
    cursor_moved: bool,
}

impl Ime {
    /// Forgets the composition: a key press ended it, and the key itself
    /// carries the text it made.
    pub(crate) fn clear(&mut self) {
        self.marked.clear();
    }

    /// True for a key press that only starts or moves a compose sequence
    /// along: a dead key, or the `o` of compose, `o`, `/`. The platform's
    /// compose handling takes the key's text away, so it reaches the pane as
    /// a printable key with no text. A dead key whose keysym has no text is
    /// named after the key under it (`'` for dead_acute on US international),
    /// and a compose sequence may show no composition at all, so this can't
    /// rely on there being one. Such a key mustn't reach the terminal as a
    /// key of its own, and mustn't end the composition. Keys with ctrl, alt
    /// or super, and named keys like the arrows, always go to the terminal.
    pub(crate) fn continues_composition(&self, ks: &Keystroke) -> bool {
        let m = &ks.modifiers;
        let mut chars = ks.key.chars();
        let one_char = chars.next().is_some() && chars.next().is_none();
        ks.key_char.is_none() && !(m.control || m.alt || m.platform) && one_char
    }

    /// Replaces the composition, which is shown but not typed.
    fn mark(&mut self, text: &str) {
        self.marked = text.to_string();
    }

    /// Ends the composition with text to type in its place. Returns the text
    /// to send, if any: an input method also commits nothing, to cancel.
    fn commit(&mut self, text: &str) -> Option<String> {
        self.marked.clear();
        (!text.is_empty()).then(|| text.to_string())
    }

    /// Ends the composition, keeping it as it stands, the way an editor keeps
    /// marked text. Returns the text to send, if any.
    fn unmark(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.marked);
        (!text.is_empty()).then_some(text)
    }

    /// The composition, counted in UTF-16 as input methods count.
    fn marked_range(&self) -> Option<Range<usize>> {
        (!self.marked.is_empty()).then(|| 0..utf16_len(&self.marked))
    }

    /// Part of the composition, the only text an input method can read
    /// back, with the range it actually covers.
    fn marked_text(&self, range: &Range<usize>) -> Option<(String, Range<usize>)> {
        let text = self.marked.get(byte_range(&self.marked, range))?.to_string();
        let start = range.start.min(utf16_len(&self.marked));
        let covered = start..start + utf16_len(&text);
        Some((text, covered))
    }

    /// Reads what to draw over the terminal, once its frame is known.
    /// `focused` is whether the pane has keyboard focus: a pane without it
    /// gets no text from the input method, so a composition left in it when
    /// focus moved on is dropped rather than drawn there for good.
    pub(crate) fn prepaint(
        &mut self,
        frame: Option<&Frame>,
        origin: Point<Pixels>,
        metrics: &Metrics,
        font: &Font,
        focused: bool,
    ) -> Overlay {
        if !focused {
            self.marked.clear();
        }
        // A hidden cursor keeps its last place, so a composition in a
        // program that hides the cursor still shows where it types.
        if let Some(c) = frame.and_then(|f| f.cursor) {
            let cell = Bounds::new(
                point(origin.x + metrics.cell_w * c.col as f32, origin.y + metrics.line_h * c.row as f32),
                size(metrics.cell_w, metrics.line_h),
            );
            if self.cursor != Some(cell) {
                self.cursor = Some(cell);
                self.cursor_moved = true;
            }
        }
        // Drawn on one line, so without line breaks or other controls.
        let text: String = self.marked.chars().filter(|c| !c.is_control()).collect();
        let preedit = match (frame, self.cursor) {
            (Some(frame), Some(cell)) if !text.is_empty() => Some(Preedit {
                text,
                at: cell.origin,
                font: font.clone(),
                font_size: metrics.font_size,
                line_h: metrics.line_h,
                fg: hsla(frame.fg),
                bg: hsla(frame.bg),
            }),
            _ => None,
        };
        // The platform is told only while the pane has focus, so a move
        // made without it is told once the pane has it again.
        let cursor_moved = focused && std::mem::take(&mut self.cursor_moved);
        Overlay { preedit, cursor_moved }
    }
}

/// What this module draws over a pane, read before painting.
pub(crate) struct Overlay {
    preedit: Option<Preedit>,
    cursor_moved: bool,
}

/// The composition in progress, drawn at the cursor and underlined, the way
/// other applications show text that isn't typed yet.
struct Preedit {
    text: String,
    at: Point<Pixels>,
    font: Font,
    font_size: Pixels,
    line_h: Pixels,
    fg: Hsla,
    bg: Hsla,
}

impl Preedit {
    fn paint(self, window: &mut Window, cx: &mut App) {
        let underline = UnderlineStyle { thickness: px(1.0), color: Some(self.fg), wavy: false };
        let run = TextRun {
            len: self.text.len(),
            font: self.font,
            color: self.fg,
            background_color: None,
            underline: Some(underline),
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(SharedString::from(self.text), self.font_size, &[run], None);
        // Cover the cells underneath, cursor included, then show a caret
        // after the composition.
        window.paint_quad(fill(Bounds::new(self.at, size(shaped.width + px(2.0), self.line_h)), self.bg));
        shaped.paint(self.at, self.line_h, window, cx).ok();
        window.paint_quad(fill(
            Bounds::new(point(self.at.x + shaped.width, self.at.y), size(px(2.0), self.line_h)),
            self.fg,
        ));
    }
}

/// Paints this module's part of a pane, after the terminal: it gives the
/// window the pane's input handler while the pane has focus, and draws any
/// composition in progress.
pub(crate) fn paint(
    pane: &Entity<Pane>,
    focus: &FocusHandle,
    bounds: Bounds<Pixels>,
    overlay: Overlay,
    window: &mut Window,
    cx: &mut App,
) {
    window.handle_input(focus, ElementInputHandler::new(bounds, pane.clone()), cx);
    if overlay.cursor_moved {
        // Tells an input method that's waiting for text where the cursor
        // is now, so its window opens there.
        window.invalidate_character_coordinates();
    }
    if let Some(preedit) = overlay.preedit {
        window.with_content_mask(Some(ContentMask { bounds }), |window| preedit.paint(window, cx));
    }
}

/// The byte range of `s` that a range of UTF-16 offsets covers, clamped to
/// `s`. Input methods count in UTF-16.
fn byte_range(s: &str, utf16: &Range<usize>) -> Range<usize> {
    let byte_at = |offset: usize| {
        let mut at = 0;
        for (i, c) in s.char_indices() {
            if at >= offset {
                return i;
            }
            at += c.len_utf16();
        }
        s.len()
    };
    let start = byte_at(utf16.start);
    start..byte_at(utf16.end).max(start)
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

impl Pane {
    /// Types text that no single key press typed.
    fn type_text(&mut self, text: &str) {
        if let Some(mirror) = &mut self.mirror {
            mirror.scroll_to_bottom();
            mirror.clear_selection();
        }
        self.control.send_nowait(
            "client.key",
            json!({"session_id": self.session_id, "block_id": self.block_id, "key": text_key(text)}),
        );
    }

    /// Scrolls the view to the previous or next shell prompt, as marked by
    /// the shell's integration (OSC 133). False when there's none to go to.
    pub(crate) fn jump_to_prompt(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let moved = self.mirror.as_mut().is_some_and(|m| m.jump_to_prompt(forward));
        if moved {
            cx.notify();
        }
        moved
    }
}

impl EntityInputHandler for Pane {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        // What's already typed belongs to the program in the terminal.
        let (text, covered) = self.ime.marked_text(&range)?;
        *adjusted_range = Some(covered);
        Some(text)
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        // The caret sits after the composition.
        let end = self.ime.marked_range().map_or(0, |r| r.end);
        Some(UTF16Selection { range: end..end, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.ime.marked_range()
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.ime.unmark() {
            self.type_text(&text);
        }
        cx.notify();
    }

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        // Committed text replaces the composition. Text already typed can't
        // be replaced, so the range doesn't matter.
        if let Some(text) = self.ime.commit(text) {
            self.type_text(&text);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime.mark(new_text);
        if let Some(mirror) = &mut self.mirror {
            // The composition is drawn at the cursor, so show the cursor.
            mirror.scroll_to_bottom();
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The cursor's cell: where the composition starts, and where the
        // input method's window should open.
        self.ime.cursor
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

/// An invisible element for the command palette's query line. The palette
/// takes keys while the pane underneath keeps keyboard focus, so without
/// this, text an input method commits would type into the terminal behind
/// the palette. Painted after the panes, its handler is the one the window
/// uses, and it sends that text to the palette's query instead.
pub(crate) fn palette_input(workspace: Entity<Workspace>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            if let Some(focused) = window.focused(cx) {
                window.handle_input(&focused, ElementInputHandler::new(bounds, workspace), cx);
            }
        },
    )
    .absolute()
    .size_full()
}

/// Text typed into the palette's query: printable, on one line.
fn query_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

impl EntityInputHandler for Workspace {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let query = &self.palette.as_ref()?.query;
        let bytes = byte_range(query, &range);
        let start = utf16_len(&query[..bytes.start]);
        let text = query[bytes].to_string();
        *adjusted_range = Some(start..start + utf16_len(&text));
        Some(text)
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        // The palette types at the end of the query.
        let end = utf16_len(&self.palette.as_ref()?.query);
        Some(UTF16Selection { range: end..end, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        // The palette doesn't show a composition in progress, only the text
        // it ends with.
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let text = query_text(text);
        let Some(p) = &mut self.palette else { return };
        if text.is_empty() {
            return;
        }
        let query = format!("{}{text}", p.query);
        p.set_query(query);
        self.refresh_suggestions(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The query line, so a candidate window opens just below it.
        Some(element_bounds)
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

/// Where a jump to a prompt goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jump {
    /// Put this row, counted from the top of the scrollback, at the top of
    /// the view.
    Row(usize),
    /// Follow the newest output again.
    Bottom,
}

/// Finds where a jump goes: the nearest prompt line above or below the top
/// of the view. `top` and `total` count rows from the top of the scrollback,
/// `len` is the view's height, and `is_prompt` tells whether a row starts a
/// prompt. A prompt too near the end to reach the top of the view scrolls to
/// the bottom instead, and `None` means the view wouldn't move.
fn prompt_jump(
    top: usize,
    len: usize,
    total: usize,
    forward: bool,
    mut is_prompt: impl FnMut(usize) -> bool,
) -> Option<Jump> {
    let lowest = total.saturating_sub(len);
    let found = if forward {
        (top + 1..total).find(|&y| is_prompt(y))?
    } else {
        (0..top.min(total)).rev().find(|&y| is_prompt(y))?
    };
    match found.min(lowest) {
        y if y == top => None,
        y if y == lowest => Some(Jump::Bottom),
        y => Some(Jump::Row(y)),
    }
}

impl Mirror {
    /// Scrolls the view so the previous or next prompt line is at its top.
    /// Only the first line of a prompt counts, so a prompt that takes two
    /// lines is one stop. False if there's no prompt that way.
    pub(crate) fn jump_to_prompt(&mut self, forward: bool) -> bool {
        let Ok(bar) = self.term.scrollbar() else { return false };
        let (total, top, len) = (bar.total as usize, bar.offset as usize, bar.len as usize);
        let term = &self.term;
        let is_prompt = |y: usize| {
            let at = GridPoint::Screen(PointCoordinate { x: 0, y: y as u32 });
            term.grid_ref(at)
                .and_then(|r| r.row())
                .and_then(|row| row.semantic_prompt())
                .is_ok_and(|p| p == RowSemanticPrompt::Prompt)
        };
        match prompt_jump(top, len, total, forward, is_prompt) {
            Some(Jump::Row(y)) => self.scroll(y as isize - top as isize),
            Some(Jump::Bottom) => self.scroll_to_bottom(),
            None => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_ranges_map_to_bytes() {
        let s = "a日😀b";
        assert_eq!(byte_range(s, &(0..1)), 0..1);
        assert_eq!(byte_range(s, &(1..2)), 1..4);
        // The emoji takes two UTF-16 units and four bytes.
        assert_eq!(byte_range(s, &(2..4)), 4..8);
        assert_eq!(byte_range(s, &(4..5)), 8..9);
        assert_eq!(byte_range(s, &(0..99)), 0..9);
        assert_eq!(byte_range(s, &(7..9)), 9..9);
        assert_eq!(byte_range("", &(0..3)), 0..0);
        assert_eq!(utf16_len(s), 5);
    }

    #[test]
    fn committed_text_is_an_unidentified_key_that_types_it() {
        let k = text_key("日本");
        assert_eq!(k["key"], "unidentified");
        assert_eq!(k["text"], "日本");
        assert_eq!(k["composing"], false);
        assert_eq!(k["mods"], "");
    }

    #[test]
    fn compositions_show_until_committed() {
        let mut ime = Ime::default();
        // An input method composes, then commits what it made.
        ime.mark("ni");
        assert_eq!(ime.marked_range(), Some(0..2));
        ime.mark("你");
        assert_eq!(ime.marked_range(), Some(0..1));
        assert_eq!(ime.marked_text(&(0..5)), Some(("你".to_string(), 0..1)));
        assert_eq!(ime.commit("你好").as_deref(), Some("你好"));
        assert_eq!(ime.marked_range(), None);
        // Cancelling deletes the composition and types nothing.
        ime.mark("´");
        assert_eq!(ime.commit(""), None);
        assert_eq!(ime.marked_range(), None);
        // Unmarking keeps the composition as typed text.
        ime.mark("~");
        assert_eq!(ime.unmark().as_deref(), Some("~"));
        assert_eq!(ime.unmark(), None);
        // A dead key's composition ends with the key that completes it.
        ime.mark("´");
        ime.clear();
        assert_eq!(ime.marked_range(), None);
    }

    #[test]
    fn keys_inside_a_compose_sequence_stay_out_of_the_terminal() {
        use gpui::Modifiers;
        let ks = |key: &str, key_char: Option<&str>, modifiers: Modifiers| Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(str::to_string),
        };
        let none = Modifiers::default();
        let mut ime = Ime::default();
        // Keys that type text go on as before.
        assert!(!ime.continues_composition(&ks("o", Some("o"), none)));
        // A dead key with no text of its own is named after the key under it,
        // and compose, o shows no composition: neither types anything yet.
        assert!(ime.continues_composition(&ks("'", None, none)));
        assert!(ime.continues_composition(&ks("o", None, none)));
        // The same while a composition is showing.
        ime.mark("o");
        assert!(ime.continues_composition(&ks("o", None, none)));
        assert!(ime.continues_composition(&ks("O", None, Modifiers { shift: true, ..none })));
        // The key that completes it carries the composed text.
        assert!(!ime.continues_composition(&ks("oslash", Some("ø"), none)));
        assert!(!ime.continues_composition(&ks("/", Some("/"), none)));
        // Commands, named keys and dead keys aren't part of the sequence.
        assert!(!ime.continues_composition(&ks("c", None, Modifiers { control: true, ..none })));
        assert!(!ime.continues_composition(&ks("x", None, Modifiers { alt: true, ..none })));
        assert!(!ime.continues_composition(&ks("left", None, none)));
        assert!(!ime.continues_composition(&ks("dead_acute", None, none)));
    }

    #[test]
    fn palette_text_is_one_printable_line() {
        assert_eq!(query_text("日本語"), "日本語");
        assert_eq!(query_text("a\nb\tc\u{7f}"), "abc");
        assert_eq!(query_text("\r"), "");
    }

    #[test]
    fn the_composition_is_drawn_at_the_cursor() {
        use crate::term::{CursorDraw, CursorShape, Rgb};
        let metrics =
            Metrics { cell_w: px(10.0), line_h: px(20.0), font_size: px(14.0), ascent: px(11.0), descent: px(3.0) };
        let frame = |cursor: Option<(u16, u16)>| Frame {
            cols: 80,
            rows: 24,
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            lines: Vec::new(),
            cursor: cursor.map(|(col, row)| CursorDraw { col, row, shape: CursorShape::Block, color: Rgb(1, 2, 3) }),
        };
        let font = gpui::font("monospace");
        let origin = point(px(4.0), px(4.0));
        let mut ime = Ime::default();
        let o = ime.prepaint(Some(&frame(Some((3, 2)))), origin, &metrics, &font, true);
        assert!(o.preedit.is_none());
        assert!(o.cursor_moved);
        let cell = Bounds::new(point(px(34.0), px(44.0)), size(px(10.0), px(20.0)));
        assert_eq!(ime.cursor, Some(cell));
        // Nothing moved, so the platform isn't told again.
        assert!(!ime.prepaint(Some(&frame(Some((3, 2)))), origin, &metrics, &font, true).cursor_moved);
        // A hidden cursor keeps its place for the composition.
        ime.mark("に");
        let o = ime.prepaint(Some(&frame(None)), origin, &metrics, &font, true);
        let preedit = o.preedit.expect("a composition to draw");
        assert_eq!((preedit.text.as_str(), preedit.at), ("に", cell.origin));
        assert!(!o.cursor_moved);
        // Line breaks aren't drawn.
        ime.mark("a\nb");
        let o = ime.prepaint(Some(&frame(None)), origin, &metrics, &font, true);
        assert_eq!(o.preedit.expect("a composition to draw").text, "ab");
        // Without focus, a move waits to be told, and the composition is
        // dropped.
        let o = ime.prepaint(Some(&frame(Some((5, 2)))), origin, &metrics, &font, false);
        assert!(o.preedit.is_none() && !o.cursor_moved);
        assert_eq!(ime.marked_range(), None);
        assert!(ime.prepaint(Some(&frame(Some((5, 2)))), origin, &metrics, &font, true).cursor_moved);
    }

    #[test]
    fn finds_the_nearest_prompt_each_way() {
        let prompts = [0, 31, 62, 93, 124];
        let is = |y: usize| prompts.contains(&y);
        // 125 rows, 24 shown, the view at the bottom.
        assert_eq!(prompt_jump(101, 24, 125, false, is), Some(Jump::Row(93)));
        assert_eq!(prompt_jump(93, 24, 125, false, is), Some(Jump::Row(62)));
        assert_eq!(prompt_jump(62, 24, 125, true, is), Some(Jump::Row(93)));
        // The last prompt can't reach the top of the view.
        assert_eq!(prompt_jump(93, 24, 125, true, is), Some(Jump::Bottom));
        assert_eq!(prompt_jump(101, 24, 125, true, is), None);
        assert_eq!(prompt_jump(0, 24, 125, false, is), None);
        // Between prompts, the one just above the view is the previous one.
        assert_eq!(prompt_jump(40, 24, 125, false, is), Some(Jump::Row(31)));
        assert_eq!(prompt_jump(40, 24, 125, true, is), Some(Jump::Row(62)));
        assert_eq!(prompt_jump(101, 24, 125, false, |_| false), None);
    }

    fn live() -> Mirror {
        let mut m = Mirror::new(1).unwrap();
        m.feed(b"\x1b[H");
        assert!(m.is_live());
        m
    }

    fn top_line(m: &mut Mirror) -> String {
        let f = m.frame().unwrap();
        f.lines[0].spans.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// A shell with integration: each prompt marked by OSC 133, its command
    /// and 30 lines of output.
    fn session(m: &mut Mirror, commands: usize) {
        for i in 0..commands {
            m.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd{i}\r\n\x1b]133;C\x07").as_bytes());
            for j in 0..30 {
                m.feed(format!("out{i} {j}\r\n").as_bytes());
            }
            m.feed(b"\x1b]133;D;0\x07");
        }
        m.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    }

    #[test]
    fn jumps_between_marked_prompts() {
        let mut m = live();
        session(&mut m, 4);
        assert_eq!(top_line(&mut m), "out3 7");
        assert!(m.jump_to_prompt(false));
        assert_eq!(top_line(&mut m), "$ cmd3");
        assert!(m.jump_to_prompt(false));
        assert_eq!(top_line(&mut m), "$ cmd2");
        assert!(m.jump_to_prompt(true));
        assert_eq!(top_line(&mut m), "$ cmd3");
        // The newest prompt is on the last screen, so next goes to the bottom.
        assert!(m.jump_to_prompt(true));
        assert_eq!(top_line(&mut m), "out3 7");
        assert!(!m.jump_to_prompt(true));
        for _ in 0..4 {
            assert!(m.jump_to_prompt(false));
        }
        assert_eq!(top_line(&mut m), "$ cmd0");
        assert!(!m.jump_to_prompt(false));
    }

    #[test]
    fn a_two_line_prompt_is_one_stop() {
        let mut m = live();
        for i in 0..3 {
            m.feed(format!("\x1b]133;A\x07~/dir{i}\r\n$ \x1b]133;B\x07cmd{i}\r\n\x1b]133;C\x07").as_bytes());
            for j in 0..30 {
                m.feed(format!("out{i} {j}\r\n").as_bytes());
            }
            m.feed(b"\x1b]133;D;0\x07");
        }
        assert!(m.jump_to_prompt(false));
        assert_eq!(top_line(&mut m), "~/dir2");
        assert!(m.jump_to_prompt(false));
        assert_eq!(top_line(&mut m), "~/dir1");
    }

    #[test]
    fn without_marks_the_view_stays() {
        let mut m = live();
        for i in 0..100 {
            m.feed(format!("$ line {i}\r\n").as_bytes());
        }
        let before = top_line(&mut m);
        assert!(!m.jump_to_prompt(false));
        assert!(!m.jump_to_prompt(true));
        assert_eq!(top_line(&mut m), before);
    }

    #[test]
    fn prompt_marks_survive_the_servers_snapshot() {
        let mut m = live();
        session(&mut m, 3);
        let bytes = ghostty_vt::snapshot::encode(&m.term).unwrap();
        m.term = ghostty_vt::snapshot::Decoder::from_slice(&bytes).decode().unwrap();
        assert!(m.jump_to_prompt(false));
        assert_eq!(top_line(&mut m), "$ cmd2");
    }
}
