//! Links in terminals: OSC 8 hyperlinks and URLs in the text, opened with Ctrl+click.
//!
//! Holding Ctrl over a link underlines it and turns the pointer into a hand;
//! Ctrl+click opens it with `xdg-open`. The click is OpenRex's, even when the
//! program has mouse tracking on, so it never reaches the program. A plain
//! click keeps selecting text, or goes to the program as before.
//!
//! Links come from two places. A program can mark text as a link with OSC 8;
//! the terminal copy records the link of each cell, and the frame carries
//! them as [`Hyperlink`] ranges on each [`Line`]. Other text is scanned for
//! `http://`, `https://`, `ftp://`, `file://`, and `mailto:` URLs, joining
//! lines that the terminal soft-wrapped so a long URL works on any of its rows.
//!
//! The server's terminal also notices link clicks: it sends a `url_clicked`
//! block event when a client sends it a mouse press on an OSC 8 link while the
//! program isn't tracking the mouse. OpenRex never sends such a press (it
//! selects text itself), so the event only ever reports another client's
//! click. It carries no client, and OpenRex receives every client's block
//! events, so opening it here would open a browser on this machine whenever
//! someone clicks a link in Rex.app or on another computer. OpenRex ignores it.

use std::process::{Command, Stdio};
use std::rc::Rc;

use ghostty_vt::Terminal;
use ghostty_vt::terminal::{Point as GridPoint, PointCoordinate};
use gpui::{
    Bounds, Context, CursorStyle, DispatchPhase, Hitbox, HitboxBehavior, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, WeakEntity, Window, fill, point, px, size,
};
use serde_json::Value;

use crate::actions::{Outcome, Target};
use crate::pane::{Metrics, Pane, hsla, path_of_pwd};
use crate::term::{Frame, Line};
use crate::workspace::Workspace;

/// Performs this module's actions. `None` for an action it doesn't own.
///
/// Links are opened with the mouse, so this module has no actions yet.
pub(crate) fn perform(
    _ws: &mut Workspace,
    _action: &str,
    _args: &Value,
    _target: &Target,
    _window: &mut Window,
    _cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    None
}

/// Cells of one row that a program made into a link with OSC 8.
#[derive(Clone, Debug, PartialEq)]
pub struct Hyperlink {
    pub col: u16,
    /// Width in cells.
    pub width: u16,
    pub uri: String,
}

/// Reads the OSC 8 link of a cell of the view. Called only for cells that
/// say they have one, while a frame is built.
pub(crate) fn hyperlink_at(term: &Terminal, x: u16, y: u32) -> Option<String> {
    let at = term.grid_ref(GridPoint::Viewport(PointCoordinate { x, y })).ok()?;
    let mut buf = vec![0u8; 256];
    let len = match at.hyperlink_uri(&mut buf) {
        Ok(len) => len,
        Err(ghostty_vt::Error::OutOfSpace { required }) => {
            buf.resize(required, 0);
            at.hyperlink_uri(&mut buf).ok()?
        }
        Err(_) => return None,
    };
    buf.truncate(len);
    let uri = String::from_utf8_lossy(&buf).into_owned();
    (!uri.is_empty()).then_some(uri)
}

/// Adds a linked cell to a row's links, joining it to the run before it.
pub(crate) fn push_hyperlink(links: &mut Vec<Hyperlink>, col: u16, width: u16, uri: String) {
    match links.last_mut() {
        Some(last) if last.uri == uri && last.col + last.width == col => last.width += width,
        _ => links.push(Hyperlink { col, width, uri }),
    }
}

/// Cells of one row, `col..col + width`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub row: u16,
    pub col: u16,
    pub width: u16,
}

/// A link found under the mouse: what it opens and the cells it covers.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub uri: String,
    pub cells: Vec<Segment>,
}

/// The link at a cell of the view, if any. An OSC 8 link wins over a URL
/// in the text.
pub fn link_at(lines: &[Rc<Line>], cols: u16, col: u16, row: u16) -> Option<Found> {
    let r = row as usize;
    let line = lines.get(r)?;
    let rows = wrapped_rows(lines, r);
    if let Some(hit) = line.links.iter().find(|l| col >= l.col && col < l.col + l.width) {
        let uri = hit.uri.as_str();
        let cells = rows
            .flat_map(|r| lines[r].links.iter().filter(move |l| l.uri == uri).map(move |l| (r, l)))
            .map(|(r, l)| Segment { row: r as u16, col: l.col, width: l.width })
            .collect();
        return Some(Found { uri: hit.uri.clone(), cells });
    }

    let last = rows.end - 1;
    let mut text = Text::default();
    for r in rows {
        text.push_row(&lines[r], r as u16, cols, r != last);
    }
    let at = text.cells.iter().position(|c| c.row == row && col >= c.col && col < c.col + c.width)?;
    let url = find_urls(&text.chars).into_iter().find(|u| u.contains(&at))?;
    let mut cells: Vec<Segment> = Vec::new();
    for c in &text.cells[url.clone()] {
        match cells.last_mut() {
            Some(last) if last.row == c.row && last.col + last.width == c.col => last.width += c.width,
            Some(last) if last.row == c.row && last.col == c.col => {}
            _ => cells.push(*c),
        }
    }
    Some(Found { uri: text.chars[url].iter().collect(), cells })
}

/// The rows that make one line of output with `row`: those the terminal
/// soft-wrapped into it, above and below.
fn wrapped_rows(lines: &[Rc<Line>], row: usize) -> std::ops::Range<usize> {
    let mut start = row;
    while start > 0 && lines[start - 1].wrapped {
        start -= 1;
    }
    let mut end = row;
    while end + 1 < lines.len() && lines[end].wrapped {
        end += 1;
    }
    start..end + 1
}

/// The text of some rows, a char at a time, with the cells each came from.
#[derive(Default)]
struct Text {
    chars: Vec<char>,
    cells: Vec<Segment>,
}

impl Text {
    fn push(&mut self, c: char, row: u16, col: u16, width: u16) {
        self.chars.push(c);
        self.cells.push(Segment { row, col, width });
    }

    /// Adds a row as drawn. Blank cells between spans become spaces, and so
    /// do the blanks at the end of a row that wraps onto the next.
    fn push_row(&mut self, line: &Line, row: u16, cols: u16, pad: bool) {
        let mut col = 0;
        for span in &line.spans {
            while col < span.col {
                self.push(' ', row, col, 1);
                col += 1;
            }
            if span.text.is_ascii() {
                for (i, c) in span.text.chars().enumerate() {
                    self.push(c, row, span.col + i as u16, 1);
                }
            } else {
                // A span that isn't plain ASCII is one cell's grapheme.
                for c in span.text.chars() {
                    self.push(c, row, span.col, span.width);
                }
            }
            col = col.max(span.col + span.width);
        }
        if pad {
            while col < cols {
                self.push(' ', row, col, 1);
                col += 1;
            }
        }
    }
}

const SCHEMES: &[&str] = &["https://", "http://", "ftp://", "file://", "mailto:"];

/// Finds the URLs in some text, as ranges of chars.
///
/// A URL starts with a known scheme at the start of a word and runs to a
/// space or a character that can't be in one. Punctuation at its end, such
/// as a full stop or the closing bracket of a parenthesis around it, is
/// left out; brackets that open inside the URL keep their closing pair.
pub fn find_urls(text: &[char]) -> Vec<std::ops::Range<usize>> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let boundary = i == 0 || !text[i - 1].is_alphanumeric();
        let scheme = boundary.then(|| SCHEMES.iter().find(|s| starts_with_ignore_case(&text[i..], s))).flatten();
        let Some(scheme) = scheme else {
            i += 1;
            continue;
        };
        let body = i + scheme.len();
        let mut end = body;
        while end < text.len() && is_url_char(text[end]) {
            end += 1;
        }
        let end = trim_url_end(text, body, end);
        if end > body {
            found.push(i..end);
            i = end;
        } else {
            i = body;
        }
    }
    found
}

fn starts_with_ignore_case(text: &[char], prefix: &str) -> bool {
    text.len() >= prefix.len() && prefix.chars().zip(text).all(|(p, t)| p.eq_ignore_ascii_case(t))
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^')
        // Box drawing and block characters: the borders of a program's UI.
        && !('\u{2500}'..='\u{259f}').contains(&c)
}

/// Where a URL that runs from `body` to `end` really ends, without the
/// punctuation of the sentence around it.
fn trim_url_end(text: &[char], body: usize, mut end: usize) -> usize {
    while end > body {
        let unbalanced = |open: char, close: char| {
            let url = &text[body..end];
            url.iter().filter(|&&c| c == close).count() > url.iter().filter(|&&c| c == open).count()
        };
        match text[end - 1] {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '*' => end -= 1,
            ')' if unbalanced('(', ')') => end -= 1,
            ']' if unbalanced('[', ']') => end -= 1,
            _ => break,
        }
    }
    end
}

/// What to give `xdg-open` for a link, or `None` for a link OpenRex won't
/// open. Only web, mail, and local file links are opened: a program could
/// write any URL into a link, and other schemes can start other programs.
/// A `file://` link names a file on the machine that wrote it, so one from
/// another host is left alone. `hostname` is this machine's.
pub fn open_target(uri: &str, hostname: &str) -> Option<String> {
    let (scheme, rest) = uri.split_once(':')?;
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" | "ftp" | "mailto" if !rest.is_empty() => Some(uri.to_string()),
        "file" => {
            let after = rest.strip_prefix("//")?;
            let host = &after[..after.find('/')?];
            let local =
                host.is_empty() || host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case(hostname);
            let path = path_of_pwd(&format!("file://{after}"));
            (local && path.starts_with('/')).then_some(path)
        }
        _ => None,
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Opens a link in the desktop's app for it.
fn open(uri: &str) {
    let Some(target) = open_target(uri, &hostname()) else {
        log::warn!("not opening a link OpenRex doesn't open: {uri}");
        return;
    };
    // Wait for the command on its own thread, so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let status = Command::new("xdg-open")
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => log::warn!("xdg-open {target}: {s}"),
            Err(e) => log::warn!("could not run xdg-open: {e}"),
        }
    });
}

/// A pane's links: what it drew last, to find the link under the mouse.
#[derive(Default)]
pub struct PaneLinks {
    lines: Vec<Rc<Line>>,
    cols: u16,
    bounds: Bounds<Pixels>,
    /// A Ctrl+click opened a link. Its release belongs to OpenRex too.
    pressed: bool,
    /// The last frame underlined a link.
    shown: bool,
}

/// A link to underline under the mouse, for painting.
pub struct Hover {
    found: Found,
    hitbox: Hitbox,
    bounds: Bounds<Pixels>,
    pane: WeakEntity<Pane>,
}

impl PaneLinks {
    /// Remembers what the pane draws this frame, and finds the link to
    /// underline: the one under the mouse while Ctrl is held.
    pub fn layout(
        &mut self,
        frame: Option<&Frame>,
        bounds: Bounds<Pixels>,
        mouse_cell: Option<(i32, i32)>,
        window: &mut Window,
        cx: &mut Context<Pane>,
    ) -> Option<Hover> {
        self.bounds = bounds;
        match frame {
            Some(frame) => {
                self.lines.clone_from(&frame.lines);
                self.cols = frame.cols;
            }
            None => self.lines.clear(),
        }
        // When the mouse leaves the window, gpui keeps its last position but
        // marks the window unhovered and draws it again.
        let over = window.is_window_hovered() && bounds.contains(&window.mouse_position());
        let found = (window.modifiers().control && over).then_some(mouse_cell).flatten().and_then(|at| self.find(at));
        self.shown = found.is_some();
        let found = found?;
        // Whether the mouse is really over this pane, and not over a pane
        // floating above it, is known only once painting starts.
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        Some(Hover { found, hitbox, bounds, pane: cx.weak_entity() })
    }

    fn find(&self, (col, row): (i32, i32)) -> Option<Found> {
        if col < 0 || row < 0 || col >= self.cols as i32 {
            return None;
        }
        link_at(&self.lines, self.cols, col as u16, row as u16)
    }

    /// Whether the pane should draw again after the mouse moved, to show or
    /// clear a link's underline.
    pub fn on_mouse_move(&self, ev: &MouseMoveEvent) -> bool {
        ev.modifiers.control || self.shown
    }

    /// Opens the link under a Ctrl+click. True if the press was on a link,
    /// so it is OpenRex's and not the program's or the selection's.
    pub fn on_mouse_down(&mut self, ev: &MouseDownEvent, cell: Option<(i32, i32)>) -> bool {
        if ev.button != MouseButton::Left {
            return false;
        }
        // A release we never heard (outside the window, say) mustn't swallow
        // the next click's.
        self.pressed = false;
        if !ev.modifiers.control {
            return false;
        }
        let Some(found) = cell.and_then(|at| self.find(at)) else { return false };
        if ev.click_count == 1 {
            open(&found.uri);
        }
        self.pressed = true;
        true
    }

    /// Whether the left button is down after opening a link. Dragging then
    /// is OpenRex's too, so it neither selects nor reaches the program.
    pub fn holds_press(&self) -> bool {
        self.pressed
    }

    /// True for the release of a press that opened a link.
    pub fn on_mouse_up(&mut self, button: MouseButton) -> bool {
        let ours = self.pressed && button == MouseButton::Left;
        if ours {
            self.pressed = false;
        }
        ours
    }

    /// Whether a change of modifier keys may change what this pane shows.
    fn wants_modifiers(&self, mouse: Point<Pixels>) -> bool {
        self.shown || self.bounds.contains(&mouse)
    }
}

/// Underlines the hovered link and shows a pointing hand over it.
pub fn paint_hover(hover: &Hover, frame: &Frame, origin: Point<Pixels>, metrics: &Metrics, window: &mut Window) {
    if !hover.hitbox.is_hovered(window) {
        return;
    }
    let Metrics { cell_w, line_h, font_size, ascent, descent } = *metrics;
    let scale = window.scale_factor();
    let snap = |v: Pixels| px((f32::from(v) * scale).round() / scale);
    let thick = px((f32::from(font_size) / 14.0 * scale).round().max(1.0) / scale);
    for seg in &hover.found.cells {
        let Some(line) = frame.lines.get(seg.row as usize) else { continue };
        let color = line
            .spans
            .iter()
            .find(|s| seg.col < s.col + s.width && s.col < seg.col + seg.width)
            .map(|s| s.fg)
            .unwrap_or(frame.fg);
        let top = origin.y + line_h * seg.row as f32;
        let baseline = top + (line_h - ascent - descent) / 2.0 + ascent;
        let y = (baseline + descent * 0.4).min(top + line_h - thick * 3.0).max(baseline + thick);
        let left = origin.x + cell_w * seg.col as f32;
        window.paint_quad(fill(Bounds::new(point(left, snap(y)), size(cell_w * seg.width as f32, thick)), hsla(color)));
    }
    window.set_cursor_style(CursorStyle::PointingHand, &hover.hitbox);
    // The pane hears mouse moves only while the mouse is over it. Draw it
    // again when the mouse leaves, to take the underline away.
    let (bounds, pane) = (hover.bounds, hover.pane.clone());
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
        if phase == DispatchPhase::Bubble && !bounds.contains(&ev.position) {
            pane.update(cx, |_, cx| cx.notify()).ok();
        }
    });
}

/// Pressing or releasing Ctrl shows or hides the link under the mouse. Only
/// the focused pane hears modifier keys, so the window passes them on to
/// whichever pane the mouse is over.
pub(crate) fn on_modifiers_changed(
    ws: &mut Workspace,
    _: &ModifiersChangedEvent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let mouse = window.mouse_position();
    let panes: Vec<_> = ws.store.read(cx).panes.values().cloned().collect();
    for pane in panes {
        pane.update(cx, |pane, cx| {
            if pane.links.wants_modifiers(mouse) {
                cx.notify();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Mirror;

    fn urls(s: &str) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        find_urls(&chars).into_iter().map(|r| chars[r].iter().collect()).collect()
    }

    #[test]
    fn finds_urls_in_text() {
        assert_eq!(urls("see https://example.com/a?b=c#d now"), ["https://example.com/a?b=c#d"]);
        assert_eq!(
            urls("http://a.b ftp://c.d/e mailto:me@x.org file:///tmp/f"),
            ["http://a.b", "ftp://c.d/e", "mailto:me@x.org", "file:///tmp/f"]
        );
        assert_eq!(urls("HTTPS://Example.COM"), ["HTTPS://Example.COM"]);
        assert_eq!(urls("no links here, just http:// and mailto:"), Vec::<String>::new());
        // Inside a word, it's not a URL.
        assert_eq!(urls("xhttp://a.b"), Vec::<String>::new());
        assert_eq!(urls("<https://a.b/c>"), ["https://a.b/c"]);
        assert_eq!(urls("\"https://a.b/c\""), ["https://a.b/c"]);
        assert_eq!(urls("│https://a.b/c│"), ["https://a.b/c"]);
        assert_eq!(urls("https://例え.jp/パス"), ["https://例え.jp/パス"]);
    }

    #[test]
    fn leaves_out_the_punctuation_around_a_url() {
        assert_eq!(urls("Go to https://a.b/c."), ["https://a.b/c"]);
        assert_eq!(urls("https://a.b/c, or https://d.e/f; then"), ["https://a.b/c", "https://d.e/f"]);
        assert_eq!(urls("really https://a.b/?!"), ["https://a.b/"]);
        assert_eq!(urls("'https://a.b/c'"), ["https://a.b/c"]);
        assert_eq!(urls("**https://a.b/c**"), ["https://a.b/c"]);
        assert_eq!(urls("(see https://a.b/c)"), ["https://a.b/c"]);
        assert_eq!(
            urls("(https://en.wikipedia.org/wiki/Rust_(language))."),
            ["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(urls("[link](https://a.b/c)"), ["https://a.b/c"]);
        assert_eq!(urls("[https://a.b/c]"), ["https://a.b/c"]);
        assert_eq!(urls("http://[::1]:8080/x"), ["http://[::1]:8080/x"]);
        assert_eq!(urls("https://a.b/x.y"), ["https://a.b/x.y"]);
    }

    #[test]
    fn chooses_what_to_open() {
        assert_eq!(open_target("https://a.b/c", "box").as_deref(), Some("https://a.b/c"));
        assert_eq!(open_target("mailto:me@x.org", "box").as_deref(), Some("mailto:me@x.org"));
        assert_eq!(open_target("file:///tmp/my%20file", "box").as_deref(), Some("/tmp/my file"));
        assert_eq!(open_target("file://box/etc/hosts", "box").as_deref(), Some("/etc/hosts"));
        assert_eq!(open_target("file://localhost/etc/hosts", "box").as_deref(), Some("/etc/hosts"));
        assert_eq!(open_target("file://elsewhere/etc/hosts", "box"), None);
        assert_eq!(open_target("file://box", "box"), None);
        assert_eq!(open_target("ssh://host", "box"), None);
        assert_eq!(open_target("javascript:alert(1)", "box"), None);
        assert_eq!(open_target("https:", "box"), None);
        assert_eq!(open_target("no scheme", "box"), None);
    }

    fn mirror(cols: u16, rows: u16) -> Mirror {
        let mut m = Mirror::new(7).unwrap();
        m.feed(b"\x1b[H");
        m.feed(format!("\x1b_rex1;7;{{\"type\":\"resize\",\"cols\":{cols},\"rows\":{rows}}}\x1b\\").as_bytes());
        m
    }

    fn at(m: &mut Mirror, col: u16, row: u16) -> Option<Found> {
        let f = m.frame().unwrap();
        link_at(&f.lines, f.cols, col, row)
    }

    #[test]
    fn carries_osc8_links_into_the_frame() {
        let mut m = mirror(40, 5);
        m.feed(b"go \x1b]8;;https://example.com/osc\x1b\\click here\x1b]8;;\x1b\\ done");
        let f = m.frame().unwrap();
        assert_eq!(f.lines[0].links, vec![Hyperlink { col: 3, width: 10, uri: "https://example.com/osc".into() }]);
        let found = link_at(&f.lines, f.cols, 8, 0).unwrap();
        assert_eq!(found.uri, "https://example.com/osc");
        assert_eq!(found.cells, vec![Segment { row: 0, col: 3, width: 10 }]);
        assert_eq!(link_at(&f.lines, f.cols, 1, 0), None);
        assert_eq!(link_at(&f.lines, f.cols, 15, 0), None);
    }

    #[test]
    fn finds_a_url_under_a_cell() {
        let mut m = mirror(40, 5);
        m.feed(b"open (https://a.b/c) now\r\nhttp://x.y");
        let found = at(&mut m, 10, 0).unwrap();
        assert_eq!(found.uri, "https://a.b/c");
        assert_eq!(found.cells, vec![Segment { row: 0, col: 6, width: 13 }]);
        assert_eq!(at(&mut m, 19, 0), None);
        assert_eq!(at(&mut m, 3, 1).unwrap().uri, "http://x.y");
        // Rows that weren't wrapped stay apart.
        assert_eq!(at(&mut m, 0, 1).unwrap().cells, vec![Segment { row: 1, col: 0, width: 10 }]);
    }

    #[test]
    fn follows_a_url_across_wrapped_rows() {
        let mut m = mirror(10, 5);
        m.feed(b"x https://abc.de/fghij k");
        let f = m.frame().unwrap();
        // "x https://" / "abc.de/fgh" / "ij k"
        assert!(f.lines[0].wrapped && f.lines[1].wrapped && !f.lines[2].wrapped);
        for (col, row) in [(4, 0), (2, 1), (1, 2)] {
            let found = link_at(&f.lines, f.cols, col, row).unwrap();
            assert_eq!(found.uri, "https://abc.de/fghij");
            assert_eq!(
                found.cells,
                vec![
                    Segment { row: 0, col: 2, width: 8 },
                    Segment { row: 1, col: 0, width: 10 },
                    Segment { row: 2, col: 0, width: 2 },
                ]
            );
        }
        assert_eq!(link_at(&f.lines, f.cols, 3, 2), None);
        assert_eq!(link_at(&f.lines, f.cols, 0, 0), None);
    }

    #[test]
    fn follows_an_osc8_link_across_wrapped_rows() {
        let mut m = mirror(10, 5);
        m.feed(b"ab\x1b]8;;https://example.com/\x1b\\0123456789\x1b]8;;\x1b\\ tail");
        let found = at(&mut m, 1, 1).unwrap();
        assert_eq!(found.uri, "https://example.com/");
        assert_eq!(found.cells, vec![Segment { row: 0, col: 2, width: 8 }, Segment { row: 1, col: 0, width: 2 }]);
    }
}
