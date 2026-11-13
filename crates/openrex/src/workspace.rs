//! The window: sessions down the side, the current session's windows as
//! tabs, and the active window's panes.

use gpui::{
    App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Entity, FocusHandle, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, actions, canvas, div, px,
    relative, rgb,
};
use openrex_proto::types::{LayoutNode, SessionView, Window as RexWindow};

use crate::store::{Status, Store, active_window};

actions!(
    openrex,
    [
        NewSession,
        NewTab,
        SplitRight,
        SplitDown,
        ClosePane,
        NextTab,
        PreviousTab,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        ToggleZoom,
        Copy,
        Paste,
        ScrollPageUp,
        ScrollPageDown,
        Reconnect,
        Quit,
    ]
);

pub const KEY_CONTEXT: &str = "OpenRex";

const SIDEBAR_W: f32 = 190.0;

struct Theme {
    bg: Hsla,
    panel: Hsla,
    border: Hsla,
    text: Hsla,
    dim: Hsla,
    accent: Hsla,
    danger: Hsla,
}

fn theme() -> Theme {
    Theme {
        bg: rgb(0x16161e).into(),
        panel: rgb(0x1a1b26).into(),
        border: rgb(0x2a2c3d).into(),
        text: rgb(0xc0caf5).into(),
        dim: rgb(0x6b7394).into(),
        accent: rgb(0x7aa2f7).into(),
        danger: rgb(0xf7768e).into(),
    }
}

/// A border between two parts of a split, being dragged.
#[derive(Clone, Debug)]
struct SplitDrag {
    horizontal: bool,
    /// The split's rectangle, as fractions of the pane area.
    rect: [f32; 4],
    before: String,
    after: String,
    sent: f32,
}

pub struct Workspace {
    store: Entity<Store>,
    focus: FocusHandle,
    /// Where the pane area was last drawn.
    area: Bounds<Pixels>,
    drag: Option<SplitDrag>,
    confirm_destroy: Option<String>,
    /// The block that last had keyboard focus in an active window.
    reported_focus: Option<String>,
    window_title: String,
}

impl Workspace {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_in(&store, window, |this, _, window, cx| {
            this.flush_clipboard(window, cx);
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |this, window, cx| this.flush_clipboard(window, cx)).detach();
        // Scripted input, for testing. See the replay module.
        if let Ok(script) = std::env::var("OPENREX_REPLAY") {
            match crate::replay::parse(&script) {
                Ok(steps) => cx
                    .spawn_in(window, async move |_, cx| {
                        cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
                        crate::replay::run(steps, cx).await;
                    })
                    .detach(),
                Err(e) => log::error!("OPENREX_REPLAY: {e}"),
            }
        }
        Workspace {
            store,
            focus: cx.focus_handle(),
            area: Bounds::default(),
            drag: None,
            confirm_destroy: None,
            reported_focus: None,
            window_title: String::new(),
        }
    }

    /// Puts text a program copied on the clipboard. A Wayland compositor
    /// ignores clipboard writes from a window without focus, so then the text
    /// goes through `wl-copy` or, without it, waits for the window to be
    /// focused.
    fn flush_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.store.update(cx, |s, _| s.pending_clipboard.take()) else { return };
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if window.is_window_active() || !wayland {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        } else if let Err(text) = wl_copy(text) {
            self.store.update(cx, |s, _| { s.pending_clipboard.get_or_insert(text); });
        }
    }

    fn with_store(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Store, &mut Context<Store>)) {
        self.store.update(cx, f);
    }

    /// Moves keyboard focus to the pane the server says is focused, and
    /// tells the server which terminal the keyboard is in.
    fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.store.update(cx, |s, _| s.take_focus_request()) {
            if let Some(pane) = self.store.read(cx).panes.get(&block) {
                let handle = pane.read(cx).focus.clone();
                window.focus(&handle);
            }
        }
        let focused = self.store.read(cx).focused_pane().filter(|p| p.read(cx).focus.is_focused(window));
        if focused.is_none() && !self.focus.contains_focused(window, cx) {
            // Keep shortcuts working when no pane has the keyboard.
            match self.store.read(cx).focused_pane() {
                Some(pane) => window.focus(&pane.read(cx).focus.clone()),
                None => window.focus(&self.focus),
            }
        }

        let now = self
            .store
            .read(cx)
            .focused_pane()
            .filter(|p| window.is_window_active() && p.read(cx).focus.is_focused(window))
            .map(|p| p.read(cx).block_id.clone());
        if now != self.reported_focus {
            let store = self.store.read(cx);
            if let Some(old) = self.reported_focus.as_ref().and_then(|id| store.panes.get(id)) {
                old.read(cx).report_focus(false);
            }
            if let Some(new) = now.as_ref().and_then(|id| store.panes.get(id)) {
                new.read(cx).report_focus(true);
            }
            self.reported_focus = now;
        }
    }

    fn on_drag_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let area = self.area;
        if area.size.width <= px(0.0) || area.size.height <= px(0.0) {
            return;
        }
        let ratio = if drag.horizontal {
            let x = (ev.position.x - area.origin.x) / area.size.width;
            (x - drag.rect[0]) / drag.rect[2]
        } else {
            let y = (ev.position.y - area.origin.y) / area.size.height;
            (y - drag.rect[1]) / drag.rect[3]
        };
        let ratio = ratio.clamp(0.05, 0.95);
        if (ratio - drag.sent).abs() < 0.004 {
            return;
        }
        drag.sent = ratio;
        let drag = drag.clone();
        let direction = if drag.horizontal { "horizontal" } else { "vertical" };
        self.with_store(cx, |s, cx| s.resize_split(direction, &drag.before, &drag.after, ratio, cx));
    }

    fn on_drag_end(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag = None;
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let store = self.store.read(cx);
        let current = store.current.clone();
        let mut list = div().flex().flex_col().flex_1().overflow_hidden();
        for (i, session) in store.sessions.iter().enumerate() {
            let active = current.as_deref() == Some(session.id.as_str());
            let confirming = self.confirm_destroy.as_deref() == Some(session.id.as_str());
            let (id, id2) = (session.id.clone(), session.id.clone());
            list = list.child(
                div()
                    .id(("session", i))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_color(if active { t.text } else { t.dim })
                    .when_active(active, t.border)
                    .hover(|s| s.bg(rgb(0x24263a)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.confirm_destroy = None;
                        let id = id.clone();
                        this.with_store(cx, |s, cx| s.select_session(id, cx));
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(session.label.clone())))
                    .child(
                        div()
                            .id(("session-close", i))
                            .px_1()
                            .text_color(if confirming { t.danger } else { t.dim })
                            .hover(|s| s.text_color(rgb(0xf7768e)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if this.confirm_destroy.as_deref() == Some(id2.as_str()) {
                                    this.confirm_destroy = None;
                                    let id = id2.clone();
                                    this.with_store(cx, |s, cx| s.destroy_session(id, cx));
                                } else {
                                    this.confirm_destroy = Some(id2.clone());
                                    cx.notify();
                                }
                            }))
                            .child(if confirming { "end?" } else { "×" }),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_W))
            .flex_shrink_0()
            .h_full()
            .bg(t.panel)
            .border_r_1()
            .border_color(t.border)
            .child(div().px_3().py_2().text_color(t.dim).text_size(px(11.0)).child("SESSIONS"))
            .child(list)
            .child(
                div()
                    .id("new-session")
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .text_color(t.dim)
                    .border_t_1()
                    .border_color(t.border)
                    .hover(|s| s.text_color(rgb(0xc0caf5)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.with_store(cx, |s, cx| s.new_session(cx));
                    }))
                    .child("+ New session"),
            )
    }

    fn render_tabs(&self, view: &SessionView, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let store = self.store.read(cx);
        let mut bar = div()
            .flex()
            .items_center()
            .h(px(30.0))
            .flex_shrink_0()
            .bg(t.panel)
            .border_b_1()
            .border_color(t.border)
            .overflow_hidden();
        for (i, win) in view.windows.iter().enumerate() {
            let Some(window_id) = win.window_id.clone() else { continue };
            let active = view.active_window_id.as_deref() == Some(window_id.as_str());
            let title = tab_title(win, store, cx);
            let (focus_id, close_id) = (window_id.clone(), window_id);
            bar = bar.child(
                div()
                    .id(("tab", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h_full()
                    .px_3()
                    .max_w(px(240.0))
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(t.border)
                    .text_color(if active { t.text } else { t.dim })
                    .when_active(active, t.bg)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let id = focus_id.clone();
                        this.with_store(cx, |s, cx| s.focus_window(id, cx));
                    }))
                    .child(div().min_w_0().truncate().child(SharedString::from(title)))
                    .child(
                        div()
                            .id(("tab-close", i))
                            .text_color(t.dim)
                            .hover(|s| s.text_color(rgb(0xf7768e)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                let id = close_id.clone();
                                this.with_store(cx, |s, cx| s.close_window(id, cx));
                            }))
                            .child("×"),
                    ),
            );
        }
        bar.child(
            div()
                .id("new-tab")
                .px_3()
                .cursor_pointer()
                .text_color(t.dim)
                .hover(|s| s.text_color(rgb(0xc0caf5)))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.with_store(cx, |s, cx| s.new_window(cx));
                }))
                .child("+"),
        )
    }

    fn render_panes(&self, view: &SessionView, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let this = cx.entity();
        let mut area = div().relative().flex_1().w_full().bg(t.bg).overflow_hidden().child(
            canvas(move |bounds, _, cx| this.update(cx, |ws, _| ws.area = bounds), |_, _, _, _| {})
                .absolute()
                .size_full(),
        );
        let Some(win) = active_window(view) else {
            return area.child(hint("This session has no windows. Press Ctrl+Shift+T for a new one."));
        };
        let store = self.store.read(cx);
        let mut dividers = Vec::new();
        for layer in &win.layers {
            if layer.is_minimized() {
                continue;
            }
            let Some(layout) = &layer.layout else { continue };
            let mut leaves = Vec::new();
            layout.leaves(&mut leaves);
            if let Some(zoomed) = &win.zoomed_block_id {
                // A zoomed block is drawn alone, over the whole window.
                leaves.retain(|(id, _)| id == zoomed);
                leaves.iter_mut().for_each(|(_, rect)| *rect = [0.0, 0.0, 1.0, 1.0]);
            } else if !layer.is_floating() {
                collect_dividers(layout, &mut dividers);
            }
            for (block_id, r) in leaves {
                let Some(pane) = store.panes.get(&block_id) else { continue };
                let focused = win.focused_block_id.as_deref() == Some(block_id.as_str())
                    && pane.read(cx).focus.is_focused(window);
                let border = match (focused, layer.is_floating()) {
                    (true, _) => t.accent,
                    (false, true) => t.dim,
                    (false, false) => t.border,
                };
                area = area.child(
                    div()
                        .absolute()
                        .left(relative(r[0]))
                        .top(relative(r[1]))
                        .w(relative(r[2]))
                        .h(relative(r[3]))
                        .border_1()
                        .border_color(border)
                        .child(pane.clone()),
                );
            }
        }
        for (i, d) in dividers.into_iter().enumerate() {
            let [x, y, w, h] = d.rect;
            let grip = div().id(("divider", i)).absolute().occlude();
            let grip = if d.horizontal {
                grip.left(relative(x + w * d.sent))
                    .top(relative(y))
                    .h(relative(h))
                    .w(px(6.0))
                    .ml(px(-3.0))
                    .cursor(CursorStyle::ResizeLeftRight)
            } else {
                grip.top(relative(y + h * d.sent))
                    .left(relative(x))
                    .w(relative(w))
                    .h(px(6.0))
                    .mt(px(-3.0))
                    .cursor(CursorStyle::ResizeUpDown)
            };
            area = area.child(grip.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.drag = Some(d.clone());
                }),
            ));
        }
        area
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let store = self.store.read(cx);
        let (dot, label) = match &store.status {
            Status::Connected => (rgb(0x9ece6a), format!("{}", store.endpoint)),
            Status::Connecting => (rgb(0xe0af68), format!("connecting to {}", store.endpoint)),
            Status::Disconnected(_) => (rgb(0xf7768e), "disconnected".to_string()),
        };
        let right = match &store.error {
            Some(e) => div().text_color(t.danger).child(SharedString::from(e.clone())),
            None => {
                let size = store
                    .focused_pane()
                    .and_then(|p| p.read(cx).grid_size())
                    .map(|(c, r)| format!("{c}×{r}"))
                    .unwrap_or_default();
                div().text_color(t.dim).child(SharedString::from(size))
            }
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(22.0))
            .flex_shrink_0()
            .px_3()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.border)
            .text_size(px(11.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_color(t.dim)
                    .child(div().size(px(7.0)).rounded_full().bg(dot))
                    .child(SharedString::from(label)),
            )
            .child(right)
    }
}

/// Styling shared by the selected row of a list.
trait WhenActive: Styled + Sized {
    fn when_active(self, active: bool, bg: Hsla) -> Self {
        if active { self.bg(bg) } else { self }
    }
}
impl<T: Styled> WhenActive for T {}

fn hint(text: &'static str) -> impl IntoElement {
    div()
        .absolute()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme().dim)
        .child(text)
}

fn tab_title(win: &RexWindow, store: &Store, cx: &App) -> String {
    let from_pane = win
        .focused_block_id
        .as_ref()
        .and_then(|id| store.panes.get(id))
        .map(|p| p.read(cx).title.clone())
        .filter(|t| !t.is_empty());
    from_pane.unwrap_or_else(|| win.label.clone())
}

/// Every border of a layout that can be dragged, with a block on each side.
fn collect_dividers(node: &LayoutNode, out: &mut Vec<SplitDrag>) {
    let Some(split) = &node.split else { return };
    let (mut before, mut after) = (Vec::new(), Vec::new());
    split.before.leaves(&mut before);
    split.after.leaves(&mut after);
    if let (Some((b, _)), Some((a, _))) = (before.last(), after.first()) {
        out.push(SplitDrag {
            horizontal: split.direction != "vertical",
            rect: node.rect,
            before: b.clone(),
            after: a.clone(),
            sent: split.ratio,
        });
    }
    collect_dividers(&split.before, out);
    collect_dividers(&split.after, out);
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_focus(window, cx);
        let t = theme();
        let store = self.store.read(cx);
        let font = SharedString::from(store.settings.font_family.clone());
        let view = store.current_view().cloned();
        let status = store.status.clone();
        let title = match view.as_ref().and_then(active_window) {
            Some(win) => format!("{} — OpenRex", tab_title(win, store, cx)),
            None => "OpenRex".to_string(),
        };
        if self.window_title != title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let main = match (&status, &view) {
            (Status::Disconnected(why), _) => div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(div().text_color(t.danger).child(SharedString::from(format!("Not connected: {why}"))))
                .child(
                    div()
                        .id("reconnect")
                        .px_3()
                        .py_1()
                        .border_1()
                        .border_color(t.border)
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(0x24263a)))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.with_store(cx, |s, cx| s.connect(cx));
                        }))
                        .child("Reconnect"),
                ),
            (_, Some(view)) => div()
                .flex_1()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(self.render_tabs(view, cx))
                .child(self.render_panes(view, window, cx)),
            (Status::Connecting, None) => div().flex_1().relative().child(hint("Connecting…")),
            (_, None) => div().flex_1().relative().child(hint("No session. Press Ctrl+Shift+N for a new one.")),
        };

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text)
            .font_family(font)
            .text_size(px(13.0))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.with_store(cx, |s, cx| s.new_session(cx))))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.with_store(cx, |s, cx| s.new_window(cx))))
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| this.with_store(cx, |s, cx| s.split("horizontal", cx))))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| this.with_store(cx, |s, cx| s.split("vertical", cx))))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| this.with_store(cx, |s, cx| s.close_focused_pane(cx))))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.with_store(cx, |s, cx| s.cycle_window(true, cx))))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| this.with_store(cx, |s, cx| s.cycle_window(false, cx))))
            .on_action(cx.listener(|this, _: &FocusLeft, _, cx| this.with_store(cx, |s, cx| s.focus_direction("left", cx))))
            .on_action(cx.listener(|this, _: &FocusRight, _, cx| this.with_store(cx, |s, cx| s.focus_direction("right", cx))))
            .on_action(cx.listener(|this, _: &FocusUp, _, cx| this.with_store(cx, |s, cx| s.focus_direction("up", cx))))
            .on_action(cx.listener(|this, _: &FocusDown, _, cx| this.with_store(cx, |s, cx| s.focus_direction("down", cx))))
            .on_action(cx.listener(|this, _: &ToggleZoom, _, cx| this.with_store(cx, |s, cx| s.toggle_zoom(cx))))
            .on_action(cx.listener(|this, _: &Reconnect, _, cx| this.with_store(cx, |s, cx| s.connect(cx))))
            .on_action(cx.listener(|this, _: &Copy, _, cx| {
                if let Some(pane) = this.store.read(cx).focused_pane() {
                    pane.update(cx, |pane, cx| pane.copy_selection(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                let text = cx.read_from_clipboard().and_then(|item| item.text());
                if let (Some(text), Some(pane)) = (text, this.store.read(cx).focused_pane()) {
                    pane.update(cx, |pane, _| pane.paste(text));
                }
            }))
            .on_action(cx.listener(|this, _: &ScrollPageUp, _, cx| {
                if let Some(pane) = this.store.read(cx).focused_pane() {
                    pane.update(cx, |pane, cx| {
                        let page = pane.grid_size().map_or(10, |(_, rows)| rows as isize - 1);
                        pane.scroll_lines(-page, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &ScrollPageDown, _, cx| {
                if let Some(pane) = this.store.read(cx).focused_pane() {
                    pane.update(cx, |pane, cx| {
                        let page = pane.grid_size().map_or(10, |(_, rows)| rows as isize - 1);
                        pane.scroll_lines(page, cx);
                    });
                }
            }))
            .child(div().flex().flex_1().overflow_hidden().child(self.render_sidebar(cx)).child(main))
            .child(self.render_status(cx))
    }
}

/// Sets the Wayland clipboard with `wl-copy`, which can do so without a
/// focused window. Gives the text back if `wl-copy` can't be started.
fn wl_copy(text: String) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = match Command::new("wl-copy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(_) => return Err(text),
    };
    let mut stdin = child.stdin.take();
    // Write and wait on its own thread, so neither blocks the window and the
    // child doesn't linger as a zombie.
    std::thread::spawn(move || {
        if let Some(stdin) = stdin.as_mut() {
            stdin.write_all(text.as_bytes()).ok();
        }
        drop(stdin);
        child.wait().ok();
    });
    Ok(())
}
