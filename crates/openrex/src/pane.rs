//! One terminal: its stream from the server, its drawing, and its input.

use std::time::Duration;

use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, ContentMask, Context, FocusHandle, Font, FontStyle,
    FontWeight, Hsla, InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, Rgba,
    ScrollDelta, ScrollWheelEvent, SharedString, Styled, Task, TextRun,
    UnderlineStyle, Window, canvas, div, fill, font, outline, point, px, size,
};
use openrex_proto::control::Control;
use openrex_proto::data::{DataConn, DataError, TERMINAL_SUBPROTOCOL};
use openrex_proto::Endpoint;
use serde_json::{Value, json};

use crate::graphics::Layer;
use crate::keys::{mods_string, rex_key};
use crate::store::Settings;
use crate::term::{CursorShape, Frame, Line, Mirror, MirrorEvent, Rgb, Span, Underline};

/// Space between the pane's edge and the grid.
const PAD: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub cell_w: Pixels,
    pub line_h: Pixels,
    pub font_size: Pixels,
    /// The font's height above and depth below the baseline.
    pub ascent: Pixels,
    pub descent: Pixels,
}

pub struct Pane {
    pub session_id: String,
    pub block_id: String,
    pub title: String,
    /// The directory the terminal's program last reported, as a path.
    pub cwd: Option<String>,
    pub exited: Option<i64>,
    pub focus: FocusHandle,
    control: Control,
    settings: Settings,
    mirror: Option<Mirror>,
    error: Option<String>,
    /// Where the grid's top-left corner was last drawn.
    origin: Point<Pixels>,
    metrics: Option<Metrics>,
    /// The size last asked of the server.
    requested: Option<(u16, u16)>,
    selecting: bool,
    /// A press began a selection, to copy when the button is released.
    copy_on_release: bool,
    reported_cell: Option<(i32, i32)>,
    scroll_remainder: f32,
    /// Textures of the Kitty graphics images in view.
    images: crate::graphics::Images,
    _stream: Task<()>,
}

struct PaintData {
    frame: Option<Frame>,
    metrics: Metrics,
    origin: Point<Pixels>,
    font: Font,
    focused: bool,
    note: Option<String>,
    images: crate::graphics::Layout,
}

impl Pane {
    pub fn new(
        session_id: String,
        block_id: String,
        control: Control,
        endpoint: Endpoint,
        settings: Settings,
        cx: &mut Context<Self>,
    ) -> Self {
        let stream = cx.spawn({
            let (control, session_id, block_id) = (control.clone(), session_id.clone(), block_id.clone());
            async move |this, cx| {
                let mut failures = 0;
                loop {
                    let result = stream_once(&control, &endpoint, &session_id, &block_id, &this, cx).await;
                    let why = match result {
                        Ok(StreamEnd::Finished) => return,
                        Ok(StreamEnd::Restart) => {
                            failures = 0;
                            None
                        }
                        Err(e) => {
                            failures += 1;
                            Some(format!("{e:#}"))
                        }
                    };
                    let give_up = failures >= 5 || control.is_closed();
                    if this
                        .update(cx, |pane, cx| {
                            pane.error = why;
                            cx.notify();
                        })
                        .is_err()
                        || give_up
                    {
                        return;
                    }
                    cx.background_executor().timer(Duration::from_millis(400)).await;
                }
            }
        });
        // Titles and directories arrive as events when they change. Ask for
        // the current ones.
        cx.spawn({
            let (control, session_id, block_id) = (control.clone(), session_id.clone(), block_id.clone());
            async move |this, cx| {
                let title = control
                    .block_call("com.superlogical.terminal.title", &session_id, &block_id, json!({}))
                    .await;
                let pwd = control.block_call("com.superlogical.terminal.pwd", &session_id, &block_id, json!({})).await;
                this.update(cx, |pane, cx| {
                    if let Some(title) = title.ok().and_then(|v| v["title"].as_str().map(str::to_string)) {
                        if pane.title.is_empty() {
                            pane.title = title;
                        }
                    }
                    if let Some(pwd) = pwd.ok().and_then(|v| v["pwd"].as_str().map(path_of_pwd)) {
                        pane.cwd.get_or_insert(pwd);
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.on_release(|pane, cx| pane.images.release(cx)).detach();
        Pane {
            session_id,
            block_id,
            title: String::new(),
            cwd: None,
            exited: None,
            focus: cx.focus_handle(),
            control,
            settings,
            mirror: None,
            error: None,
            origin: Point::default(),
            metrics: None,
            requested: None,
            selecting: false,
            copy_on_release: false,
            reported_cell: None,
            scroll_remainder: 0.0,
            images: Default::default(),
            _stream: stream,
        }
    }

    fn on_stream_open(&mut self, nonce: u64, cx: &mut Context<Self>) {
        match Mirror::new(nonce) {
            Ok(mirror) => {
                self.mirror = Some(mirror);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        self.requested = None;
        cx.notify();
    }

    fn on_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(mirror) = &mut self.mirror else { return };
        for event in mirror.feed(bytes) {
            match event {
                MirrorEvent::Exited { exit_code } => self.exited = Some(exit_code),
                MirrorEvent::Resized { .. } => {}
            }
        }
        cx.notify();
    }

    /// The terminal's size in cells, once known.
    pub fn grid_size(&self) -> Option<(u16, u16)> {
        self.mirror.as_ref().filter(|m| m.is_live()).map(|m| m.size())
    }

    fn block_event(&self, kind: &str, payload: Value) {
        self.control.send_nowait(
            "block.event",
            json!({
                "session_id": self.session_id,
                "block_id": self.block_id,
                "event": {"type": kind, "payload": payload},
            }),
        );
    }

    pub fn paste(&mut self, text: String) {
        if !text.is_empty() {
            if let Some(mirror) = &mut self.mirror {
                mirror.scroll_to_bottom();
            }
            self.block_event("paste", json!({"text": text}));
        }
    }

    /// Copies the selection to the clipboard. False if nothing is selected.
    pub fn copy_selection(&self, cx: &mut App) -> bool {
        match self.mirror.as_ref().and_then(|m| m.selection_text()) {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(mirror) = &mut self.mirror {
            mirror.select_all();
            if let Some(text) = mirror.selection_text() {
                cx.write_to_primary(ClipboardItem::new_string(text));
            }
            cx.notify();
        }
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.settings.font_size = size;
        cx.notify();
    }

    pub fn scroll_lines(&mut self, lines: isize, cx: &mut Context<Self>) {
        if let Some(mirror) = &mut self.mirror {
            mirror.scroll(lines);
            cx.notify();
        }
    }

    /// Tells the server this pane has, or lost, keyboard focus.
    pub fn report_focus(&self, focused: bool) {
        self.block_event("focus", json!({"action": if focused { "in" } else { "out" }}));
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<(i32, i32)> {
        let m = self.metrics?;
        let col = ((position.x - self.origin.x) / m.cell_w).floor() as i32;
        let row = ((position.y - self.origin.y) / m.line_h).floor() as i32;
        Some((col, row))
    }

    fn wants_mouse(&self, modifiers: &Modifiers) -> bool {
        // Holding shift keeps the mouse for selecting, as in other terminals.
        !modifiers.shift && self.mirror.as_ref().is_some_and(|m| m.is_mouse_tracking())
    }

    fn send_mouse(&mut self, action: &str, button: Option<MouseButton>, position: Point<Pixels>, modifiers: &Modifiers) {
        let Some((col, row)) = self.cell_at(position) else { return };
        let Some((cols, rows)) = self.grid_size() else { return };
        let (col, row) = (col.clamp(0, cols as i32 - 1), row.clamp(0, rows as i32 - 1));
        if action == "motion" {
            if self.reported_cell == Some((col, row)) {
                return;
            }
        }
        self.reported_cell = Some((col, row));
        let name = button.and_then(button_name);
        let held = match (action, name) {
            ("release", _) | (_, None) => Value::Null,
            (_, Some(name)) => json!({name: true}),
        };
        self.block_event(
            "mouse",
            json!({
                "action": action,
                "button": name.unwrap_or("unknown"),
                "buttons": held,
                "x": col,
                "y": row,
                "mods": mods_string(modifiers),
            }),
        );
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = rex_key(&ev.keystroke, ev.is_held) else { return };
        if let Some(mirror) = &mut self.mirror {
            mirror.scroll_to_bottom();
            mirror.clear_selection();
        }
        self.control.send_nowait(
            "client.key",
            json!({"session_id": self.session_id, "block_id": self.block_id, "key": key}),
        );
        cx.stop_propagation();
        cx.notify();
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            window.focus(&self.focus);
            self.control.send_nowait(
                "session.focus_block",
                json!({"session_id": self.session_id, "block_id": self.block_id}),
            );
        }
        if self.wants_mouse(&ev.modifiers) {
            self.reported_cell = None;
            self.send_mouse("press", Some(ev.button), ev.position, &ev.modifiers);
            return;
        }
        match ev.button {
            MouseButton::Left => {
                if let (Some((col, row)), Some(mirror)) = (self.cell_at(ev.position), &mut self.mirror) {
                    match ev.click_count {
                        2 => mirror.select_word(col, row),
                        3 => mirror.select_line(col, row),
                        _ => {
                            mirror.select_start(col, row);
                            self.selecting = true;
                        }
                    }
                    self.copy_on_release = true;
                }
            }
            MouseButton::Middle => {
                if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                    self.paste(text);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            if let (Some((col, row)), Some(mirror)) = (self.cell_at(ev.position), &mut self.mirror) {
                mirror.select_to(col, row);
                cx.notify();
            }
        } else if ev.pressed_button.is_some() && self.wants_mouse(&ev.modifiers) {
            self.send_mouse("motion", ev.pressed_button, ev.position, &ev.modifiers);
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting || self.copy_on_release {
            self.selecting = false;
            self.copy_on_release = false;
            if let Some(text) = self.mirror.as_ref().and_then(|m| m.selection_text()) {
                cx.write_to_primary(ClipboardItem::new_string(text));
            }
            cx.notify();
        } else if self.wants_mouse(&ev.modifiers) {
            self.send_mouse("release", Some(ev.button), ev.position, &ev.modifiers);
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(metrics) = self.metrics else { return };
        // Positive means the wheel moved up, toward older output.
        let lines = match ev.delta {
            ScrollDelta::Lines(d) => d.y * 3.0,
            ScrollDelta::Pixels(d) => d.y / metrics.line_h,
        } + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        if whole == 0.0 {
            return;
        }
        let to_program = self.mirror.as_ref().is_some_and(|m| m.is_mouse_tracking() || m.is_alt_screen());
        if to_program && !ev.modifiers.shift {
            let (col, row) = self.cell_at(ev.position).unwrap_or((0, 0));
            self.block_event(
                "wheel",
                json!({
                    "delta_mode": "line",
                    "delta_x": 0,
                    "delta_y": -whole,
                    "x": col.max(0),
                    "y": row.max(0),
                    "mods": mods_string(&ev.modifiers),
                }),
            );
        } else {
            self.scroll_lines(-(whole as isize), cx);
        }
    }

    /// Measures the pane, asks the server for the size that fits, and reads
    /// what to draw.
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window) -> PaintData {
        let font = font(SharedString::from(self.settings.font_family.clone()));
        let font_size = px(self.settings.font_size);
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_w = text_system
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(font_size * 0.6);
        let line_h = (font_size * 1.3).round();
        let ascent = text_system.ascent(font_id, font_size);
        let descent = text_system.descent(font_id, font_size).abs();
        let metrics = Metrics { cell_w, line_h, font_size, ascent, descent };
        let origin = bounds.origin + point(px(PAD), px(PAD));
        self.metrics = Some(metrics);
        self.origin = origin;

        let fit_cols = ((bounds.size.width - px(2.0 * PAD)) / cell_w).floor().max(2.0) as u16;
        let fit_rows = ((bounds.size.height - px(2.0 * PAD)) / line_h).floor().max(1.0) as u16;
        let fit = (fit_cols, fit_rows);
        if let Some(current) = self.grid_size() {
            if current != fit && self.requested != Some(fit) && self.exited.is_none() {
                self.requested = Some(fit);
                self.control.send_nowait(
                    "com.superlogical.terminal.resize",
                    json!({
                        "session_id": self.session_id,
                        "block_id": self.block_id,
                        "args": {
                            "columns": fit_cols,
                            "rows": fit_rows,
                            "cell_width_px": (cell_w / px(1.0)).round().max(1.0) as u32,
                            "cell_height_px": (line_h / px(1.0)).round().max(1.0) as u32,
                        },
                    }),
                );
            } else if current == fit {
                self.requested = None;
            }
        }

        let frame = self.mirror.as_mut().filter(|m| m.is_live()).and_then(|m| match m.frame() {
            Ok(frame) => Some(frame),
            Err(e) => {
                log::error!("reading the terminal: {e:#}");
                None
            }
        });
        let note = match (&self.error, self.exited) {
            (Some(e), _) => Some(e.clone()),
            (None, Some(code)) => Some(format!("[process exited with code {code}]")),
            (None, None) if frame.is_none() => Some("connecting…".to_string()),
            _ => None,
        };
        let images = self.images.layout(self.mirror.as_ref(), origin, &metrics, window);
        PaintData { frame, metrics, origin, font, focused: self.focus.is_focused(window), note, images }
    }
}

enum StreamEnd {
    /// The stream is over for good.
    Finished,
    /// The stream needs a new ticket.
    Restart,
}

/// Runs one data stream until it ends.
async fn stream_once(
    control: &Control,
    endpoint: &Endpoint,
    session_id: &str,
    block_id: &str,
    this: &gpui::WeakEntity<Pane>,
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<StreamEnd> {
    let ticket = control
        .call(
            "data.connect",
            json!({
                "session_id": session_id,
                "block_id": block_id,
                "data_ref_id": "terminal",
                "subprotocol": TERMINAL_SUBPROTOCOL,
            }),
        )
        .await;
    let ticket = match ticket {
        Ok(v) => v,
        // The block is gone, or the session was detached.
        Err(e) if e.status == 404 => return Ok(StreamEnd::Finished),
        Err(e) => return Err(e.into()),
    };
    let ticket = ticket["ticket"].as_str().unwrap_or_default().to_string();
    let mut conn = DataConn::open(endpoint, TERMINAL_SUBPROTOCOL, &ticket).await?;
    let nonce = conn.nonce;
    if this.update(cx, |pane, cx| pane.on_stream_open(nonce, cx)).is_err() {
        return Ok(StreamEnd::Finished);
    }
    loop {
        match conn.next().await {
            Ok(Some(bytes)) => {
                if this.update(cx, |pane, cx| pane.on_bytes(&bytes, cx)).is_err() {
                    conn.close().await;
                    return Ok(StreamEnd::Finished);
                }
            }
            Ok(None) => return Ok(StreamEnd::Finished),
            Err(e) => match e.downcast_ref::<DataError>() {
                Some(DataError::BlockClosed) => return Ok(StreamEnd::Finished),
                Some(_) => return Ok(StreamEnd::Restart),
                // The connection dropped. Continue the stream where it stopped.
                None => {
                    if conn.resume().await.is_err() {
                        return Ok(StreamEnd::Restart);
                    }
                }
            },
        }
    }
}

fn button_name(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Left => Some("left"),
        MouseButton::Right => Some("right"),
        MouseButton::Middle => Some("middle"),
        _ => None,
    }
}

pub fn hsla(c: Rgb) -> Hsla {
    Rgba { r: c.0 as f32 / 255.0, g: c.1 as f32 / 255.0, b: c.2 as f32 / 255.0, a: 1.0 }.into()
}

/// The text of the cell at `col`, with the style it is drawn in.
fn glyph_at(line: &Line, col: u16) -> Option<(String, bool, bool)> {
    let span = line.spans.iter().find(|s| col >= s.col && col < s.col + s.width)?;
    let text = if span.text.is_ascii() {
        let i = (col - span.col) as usize;
        span.text.get(i..i + 1)?.to_string()
    } else {
        span.text.clone()
    };
    (!text.trim().is_empty()).then_some((text, span.bold, span.italic))
}

/// Draws a span's underline, strikethrough, and overline across exactly the
/// cells it covers. `text_color` is the color its glyphs are drawn in.
fn paint_decorations(
    span: &Span,
    text_color: Hsla,
    left: Pixels,
    right: Pixels,
    top: Pixels,
    metrics: &Metrics,
    window: &mut Window,
) {
    let Metrics { cell_w, line_h, font_size, ascent, descent } = *metrics;
    // Lines sit on whole device pixels, so a fractional display scale
    // doesn't blur them, and are at least one device pixel thick.
    let scale = window.scale_factor();
    let snap = |v: Pixels| px((f32::from(v) * scale).round() / scale);
    let thick = px((f32::from(font_size) / 14.0 * scale).round().max(1.0) / scale);
    let width = right - left;
    let baseline = top + (line_h - ascent - descent) / 2.0 + ascent;
    let bar = |window: &mut Window, x: Pixels, y: Pixels, w: Pixels, color: Hsla| {
        window.paint_quad(fill(Bounds::new(point(x, snap(y)), size(w, thick)), color));
    };

    if span.underline != Underline::None {
        let color = span.underline_color.map(hsla).unwrap_or(text_color);
        // Below the baseline, but never below the cell, where the next row
        // would draw over it.
        let lowest = top + line_h - thick * 3.0;
        let y = (baseline + descent * 0.4).min(lowest).max(baseline + thick);
        match span.underline {
            Underline::Double => {
                bar(window, left, y - thick, width, color);
                bar(window, left, y + thick * 1.5, width, color);
            }
            Underline::Curly => {
                let style = UnderlineStyle { thickness: thick, color: Some(color), wavy: true };
                window.paint_underline(point(left, y - thick), width, &style);
            }
            Underline::Dotted => {
                let mut x = left;
                while x < right {
                    bar(window, x, y, thick.min(right - x), color);
                    x += thick * 2.0;
                }
            }
            Underline::Dashed => {
                let cells = (width / cell_w).round() as u16;
                for i in 0..cells {
                    let x = left + cell_w * i as f32;
                    bar(window, x + cell_w * 0.15, y, cell_w * 0.7, color);
                }
            }
            _ => bar(window, left, y, width, color),
        }
    }
    if span.strike {
        bar(window, left, baseline - ascent * 0.3, width, text_color);
    }
    if span.overline {
        bar(window, left, top, width, text_color);
    }
}

fn styled(base: &Font, bold: bool, italic: bool) -> Font {
    Font {
        weight: if bold { FontWeight::BOLD } else { base.weight },
        style: if italic { FontStyle::Italic } else { base.style },
        ..base.clone()
    }
}

fn paint(bounds: Bounds<Pixels>, data: PaintData, window: &mut Window, cx: &mut App) {
    let PaintData { frame, metrics, origin, font, focused, note, images } = data;
    let Metrics { cell_w, line_h, font_size, .. } = metrics;
    let bg = frame.as_ref().map(|f| hsla(f.bg)).unwrap_or_else(|| hsla(Rgb(0x16, 0x16, 0x1e)));
    let fg = frame.as_ref().map(|f| hsla(f.fg)).unwrap_or_else(|| hsla(Rgb(0xc0, 0xca, 0xf5)));

    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        window.paint_quad(fill(bounds, bg));
        images.paint(Layer::BelowBackground, window);
        let x_of = |col: u16| origin.x + cell_w * col as f32;

        if let Some(frame) = &frame {
            for (row, line) in frame.lines.iter().enumerate() {
                let top = origin.y + line_h * row as f32;
                for span in &line.bgs {
                    let (left, right) = (x_of(span.col).floor(), x_of(span.col + span.width).ceil());
                    window.paint_quad(fill(
                        Bounds::new(point(left, top), size(right - left, line_h)),
                        hsla(span.color),
                    ));
                }
            }
            // Every row's backgrounds go down before any text, so an image
            // below the text still covers the backgrounds of all its rows.
            images.paint(Layer::BelowText, window);
            for (row, line) in frame.lines.iter().enumerate() {
                let top = origin.y + line_h * row as f32;
                for span in &line.spans {
                    let mut color = hsla(span.fg);
                    if span.faint {
                        color.a = 0.6;
                    }
                    let run = TextRun {
                        len: span.text.len(),
                        font: styled(&font, span.bold, span.italic),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    // Plain ASCII is pinned to the grid, one glyph per cell.
                    let pin = span.text.is_ascii().then_some(cell_w);
                    let shaped = window.text_system().shape_line(
                        SharedString::from(span.text.clone()),
                        font_size,
                        &[run],
                        pin,
                    );
                    shaped.paint(point(x_of(span.col), top), line_h, window, cx).ok();
                    if span.decorated() {
                        let (left, right) = (x_of(span.col), x_of(span.col + span.width));
                        paint_decorations(span, color, left, right, top, &metrics, window);
                    }
                }
            }
            images.paint(Layer::AboveText, window);

            if let Some(cursor) = frame.cursor.filter(|c| (c.row as usize) < frame.lines.len()) {
                let top = origin.y + line_h * cursor.row as f32;
                let cell = Bounds::new(point(x_of(cursor.col), top), size(cell_w, line_h));
                let color = hsla(cursor.color);
                let shape = if focused { cursor.shape } else { CursorShape::HollowBlock };
                match shape {
                    CursorShape::Block => {
                        window.paint_quad(fill(cell, color));
                        if let Some((text, bold, italic)) = glyph_at(&frame.lines[cursor.row as usize], cursor.col) {
                            let run = TextRun {
                                len: text.len(),
                                font: styled(&font, bold, italic),
                                color: bg,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            let shaped =
                                window.text_system().shape_line(SharedString::from(text), font_size, &[run], None);
                            shaped.paint(cell.origin, line_h, window, cx).ok();
                        }
                    }
                    CursorShape::HollowBlock => {
                        window.paint_quad(outline(cell, color, BorderStyle::Solid));
                    }
                    CursorShape::Bar => {
                        window.paint_quad(fill(Bounds::new(cell.origin, size(px(2.0), line_h)), color));
                    }
                    CursorShape::Underline => {
                        let y = top + line_h - px(2.0);
                        window.paint_quad(fill(Bounds::new(point(cell.origin.x, y), size(cell_w, px(2.0))), color));
                    }
                }
            }
        }

        if let Some(note) = note {
            let mut color = fg;
            color.a = 0.55;
            let run = TextRun {
                len: note.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(SharedString::from(note), font_size, &[run], None);
            let y = bounds.origin.y + bounds.size.height - line_h - px(PAD);
            shaped.paint(point(origin.x, y.max(origin.y)), line_h, window, cx).ok();
        }
    });
}

impl Render for Pane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, window, cx| entity.update(cx, |pane, _| pane.layout(bounds, window)),
                    paint,
                )
                .size_full(),
            )
    }
}

/// A reported directory as a path: OSC 7 sends a `file://host/path` URL.
pub fn path_of_pwd(pwd: &str) -> String {
    let Some(rest) = pwd.strip_prefix("file://") else { return pwd.to_string() };
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
    percent_decode(path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_reported_directories() {
        assert_eq!(path_of_pwd("file://host/home/me/my%20dir"), "/home/me/my dir");
        assert_eq!(path_of_pwd("/plain/path"), "/plain/path");
        assert_eq!(path_of_pwd("file:///tmp"), "/tmp");
    }
}
