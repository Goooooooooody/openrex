//! The window: sessions down the side, the current session's windows as
//! tabs, and the active window's panes.

use gpui::{
    App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Entity, FocusHandle, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, canvas, div, hsla, px,
    relative, rgb,
};
use openrex_proto::types::{LayoutNode, SessionView, Window as RexWindow};
use serde_json::Value;

use crate::actions::Target;
use crate::keymap::{self, ActionInfo, Press, Resolution};
use crate::palette::{self, Palette};
use crate::store::{Status, Store, active_window};

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
    pub(crate) store: Entity<Store>,
    focus: FocusHandle,
    /// Where the pane area was last drawn.
    area: Bounds<Pixels>,
    drag: Option<SplitDrag>,
    confirm_destroy: Option<String>,
    /// The block that last had keyboard focus in an active window.
    reported_focus: Option<String>,
    window_title: String,
    pub(crate) palette: Option<Palette>,
    /// The key modes entered with `client.mode.enter`, innermost last.
    pub(crate) modes: Vec<String>,
    /// The keys of an unfinished key sequence.
    key_prefix: Vec<Press>,
    pub(crate) client_actions: Vec<ActionInfo>,
    /// Actions a configuration file action asked this client to perform.
    pub(crate) queued_actions: Vec<(String, Value)>,
    pub(crate) sidebar_visible: bool,
    pub(crate) font_size: f32,
    pub(crate) base_font_size: f32,
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
        let font_size = store.read(cx).settings.font_size;
        Workspace {
            store,
            focus: cx.focus_handle(),
            area: Bounds::default(),
            drag: None,
            confirm_destroy: None,
            reported_focus: None,
            window_title: String::new(),
            palette: None,
            modes: Vec::new(),
            key_prefix: Vec::new(),
            client_actions: keymap::client_actions(),
            queued_actions: Vec::new(),
            sidebar_visible: true,
            font_size,
            base_font_size: font_size,
        }
    }

    /// Performs what other clients asked for and what configuration file
    /// actions queued.
    fn drain_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let requests = self.store.update(cx, |s, _| std::mem::take(&mut s.perform_queue));
        for req in requests {
            let target = Target {
                session_id: req.session_id.clone(),
                block_id: req.block_id.clone(),
                origin: req.origin.clone(),
                ..Default::default()
            };
            let outcome = if self.client_actions.iter().any(|a| a.name == req.action) {
                self.perform(&req.action, &req.args, target, window, cx)
            } else {
                Err(("not_supported".to_string(), format!("OpenRex has no action named {}", req.action)))
            };
            self.store.read(cx).reply(&req.call_id, outcome);
        }
        for (action, args) in std::mem::take(&mut self.queued_actions) {
            let target = Target { origin: "script".into(), ..Default::default() };
            let outcome = self.perform(&action, &args, target, window, cx);
            self.report(outcome, &action, cx);
        }
    }

    /// Shows an action's failure in the status bar.
    fn report(&mut self, outcome: crate::actions::Outcome, action: &str, cx: &mut Context<Self>) {
        if let Err((_, message)) = outcome {
            let action = action.to_string();
            self.store.update(cx, |s, cx| {
                s.error = Some(format!("{action}: {message}"));
                cx.notify();
            });
        }
    }

    /// Hook for actions that live in their own modules.
    pub(crate) fn perform_extra(
        &mut self,
        action: &str,
        args: &Value,
        target: &Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<crate::actions::Outcome> {
        type Perform = fn(&mut Workspace, &str, &Value, &Target, &mut Window, &mut Context<Workspace>) -> Option<crate::actions::Outcome>;
        const MODULES: &[Perform] = &[
            crate::find::perform,
            crate::links::perform,
            crate::activity::perform,
            crate::layers::perform,
            crate::transfer::perform,
            crate::theme::perform,
            crate::server::perform,
            crate::input::perform,
            crate::graphics::perform,
        ];
        MODULES.iter().find_map(|perform| perform(self, action, args, target, window, cx))
    }

    /// Every key press passes here first: the palette takes it while open,
    /// then the keymap, and only an unbound key reaches the terminal.
    fn on_key_capture(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            cx.stop_propagation();
            self.palette_key(ev, window, cx);
            cx.notify();
            return;
        }
        let Some(press) = keymap::press_of(&ev.keystroke) else { return };
        let resolution = {
            let store = self.store.read(cx);
            store.keymap.resolve(&self.modes, &self.key_prefix, &press, |a| self.can_perform(a, cx))
        };
        match resolution {
            Resolution::Run(binding) => {
                cx.stop_propagation();
                let prefix: Vec<String> = self.key_prefix.drain(..).map(|p| p.spelled()).collect();
                let target = Target {
                    origin: "key".into(),
                    key: crate::keys::rex_key(&ev.keystroke, ev.is_held),
                    prefix,
                    ..Default::default()
                };
                let outcome = self.perform(&binding.action, &binding.args, target, window, cx);
                self.report(outcome, &binding.action, cx);
            }
            Resolution::Pending => {
                cx.stop_propagation();
                self.key_prefix.push(press);
            }
            Resolution::Swallow => {
                cx.stop_propagation();
                self.key_prefix.clear();
            }
            Resolution::Unbound => self.key_prefix.clear(),
        }
        cx.notify();
    }

    fn palette_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        let Some(p) = &mut self.palette else { return };
        let before = p.query.clone();
        match ks.key.as_str() {
            "escape" => self.palette = None,
            "enter" => {
                if let Some((action, args)) = p.submit() {
                    self.palette = None;
                    let target = Target { origin: "palette".into(), ..Default::default() };
                    let outcome = self.perform(&action, &args, target, window, cx);
                    self.report(outcome, &action, cx);
                }
            }
            "up" => p.move_selection(-1),
            "down" => p.move_selection(1),
            "pageup" => p.move_selection(-10),
            "pagedown" => p.move_selection(10),
            "tab" => p.complete(),
            "backspace" if m.control || m.alt => {
                let mut q = p.query.clone();
                palette::delete_word(&mut q);
                p.set_query(q);
            }
            "backspace" => {
                let mut q = p.query.clone();
                q.pop();
                p.set_query(q);
            }
            "p" if m.control => p.move_selection(-1),
            "n" if m.control => p.move_selection(1),
            "u" if m.control => p.set_query(String::new()),
            "w" if m.control => {
                let mut q = p.query.clone();
                palette::delete_word(&mut q);
                p.set_query(q);
            }
            "v" if m.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    let line = text.lines().next().unwrap_or("").to_string();
                    let q = format!("{}{line}", p.query);
                    p.set_query(q);
                }
            }
            _ if !m.control && !m.platform => {
                if let Some(text) = ks.key_char.as_ref().filter(|t| !t.chars().any(char::is_control)) {
                    let q = format!("{}{text}", p.query);
                    p.set_query(q);
                }
            }
            _ => {}
        }
        if self.palette.as_ref().is_some_and(|p| p.query != before) {
            self.refresh_suggestions(cx);
        }
    }

    pub(crate) fn window_title(&self, win: &RexWindow, cx: &App) -> String {
        tab_title(win, self.store.read(cx), cx)
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

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const ROWS: usize = 14;
        let t = theme();
        let Some(p) = &self.palette else { return div().id("palette-backdrop") };
        let visible = p.visible();
        let selected = p.selected.min(visible.len().saturating_sub(1));
        let first = selected.saturating_sub(ROWS - 1);
        let mut list = div().flex().flex_col().py_1();
        for (i, item) in visible.iter().enumerate().skip(first).take(ROWS) {
            let chosen = i == selected;
            list = list.child(
                div()
                    .id(("palette-item", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(3.0))
                    .cursor_pointer()
                    .when_active(chosen, rgb(0x2e3250).into())
                    .hover(|s| s.bg(rgb(0x24263a)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let Some(p) = &mut this.palette else { return };
                        p.selected = i;
                        if let Some((action, args)) = p.submit() {
                            this.palette = None;
                            let target = Target { origin: "palette".into(), ..Default::default() };
                            let outcome = this.perform(&action, &args, target, window, cx);
                            this.report(outcome, &action, cx);
                        }
                        cx.notify();
                    }))
                    .child(div().flex_shrink_0().text_color(if chosen { t.text } else { rgb(0xa9b1d6).into() }).child(SharedString::from(item.label.clone())))
                    .child(div().flex_1().min_w_0().truncate().text_color(t.dim).text_size(px(11.0)).child(SharedString::from(item.detail.clone())))
                    .child(div().flex_shrink_0().text_color(t.dim).text_size(px(11.0)).child(SharedString::from(item.hint.clone()))),
            );
        }
        if visible.is_empty() && !p.empty.is_empty() {
            list = list.child(div().px_3().py_1().text_color(t.dim).child(SharedString::from(p.empty.clone())));
        }
        let (query, color) = if p.query.is_empty() {
            (p.placeholder.clone(), t.dim)
        } else {
            (p.query.clone(), t.text)
        };
        let count = if p.is_input() || visible.len() <= ROWS { String::new() } else { format!("{} of {}", selected + 1, visible.len()) };
        div()
            .id("palette-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .pt(px(70.0))
            .bg(hsla(0.0, 0.0, 0.0, 0.35))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.palette = None;
                cx.notify();
            }))
            .child(
                div()
                    .id("palette")
                    .w(px(640.0))
                    .max_h(px(520.0))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .bg(t.panel)
                    .border_1()
                    .border_color(t.accent)
                    .rounded_md()
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .px_3()
                            .pt_2()
                            .text_size(px(11.0))
                            .text_color(t.dim)
                            .child(SharedString::from(p.title.clone()))
                            .child(SharedString::from(count)),
                    )
                    .child(
                        div()
                            .flex()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(t.border)
                            .text_color(color)
                            .child(SharedString::from(query))
                            .child(div().w(px(2.0)).h(px(16.0)).ml(px(1.0)).bg(t.accent))
                            .child(crate::input::palette_input(cx.entity())),
                    )
                    .child(list),
            )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let store = self.store.read(cx);
        let (dot, label) = match &store.status {
            Status::Connected => (rgb(0x9ece6a), format!("{}", store.endpoint)),
            Status::Connecting => (rgb(0xe0af68), format!("connecting to {}", store.endpoint)),
            Status::Disconnected(_) => (rgb(0xf7768e), "disconnected".to_string()),
        };
        let mut keys_state = String::new();
        if let Some(mode) = self.modes.last() {
            keys_state = format!("mode: {mode}");
        }
        if !self.key_prefix.is_empty() {
            let typed: Vec<String> = self.key_prefix.iter().map(|p| p.spelled()).collect();
            keys_state = format!("{keys_state}  {} …", typed.join(" ")).trim().to_string();
        }
        let right = match (&store.error, &store.notice) {
            (Some(e), _) => div().text_color(t.danger).child(SharedString::from(e.clone())),
            (None, Some(n)) => div().text_color(t.accent).child(SharedString::from(n.clone())),
            (None, None) => {
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
                    .child(SharedString::from(label))
                    .when_some_el((!keys_state.is_empty()).then(|| div().text_color(t.accent).child(SharedString::from(keys_state)))),
            )
            .child(right)
    }
}

/// Adds a child only when there is one.
trait WhenSomeEl: ParentElement + Sized {
    fn when_some_el(self, child: Option<impl IntoElement>) -> Self {
        match child {
            Some(c) => self.child(c),
            None => self,
        }
    }
}
impl<T: ParentElement> WhenSomeEl for T {}

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
        self.drain_requests(window, cx);
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
            .capture_key_down(cx.listener(Self::on_key_capture))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .when_some_el(self.sidebar_visible.then(|| self.render_sidebar(cx)))
                    .child(main),
            )
            .child(self.render_status(cx))
            .when_some_el(self.palette.is_some().then(|| self.render_palette(cx)))
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
