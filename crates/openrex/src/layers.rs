//! Floating layers: drawing them over the layout, moving, resizing, raising, minimizing, and closing them.
//!
//! The server keeps each floating layer's place as `[x, y, w, h]` fractions
//! of the window, and the rectangles of the blocks inside it in the same
//! window fractions, so a layer's own layout fills its bounds exactly. A
//! layer is drawn at its bounds with a slim title bar across its top, and its
//! blocks share the space under the bar in the proportions the server gave.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BoxShadow, Context, CursorStyle, DispatchPhase, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement,
    Styled, Window, canvas, div, hsla, point, px, relative, rgb,
};
use openrex_proto::types::{Layer, SessionView, Window as RexWindow};
use serde_json::{Value, json};

use crate::actions::{Outcome, Target, tiled_leaves};
use crate::store::active_window;
use crate::workspace::Workspace;

/// Height of a floating layer's title bar.
const TITLE_H: f32 = 22.0;
/// The smallest a layer can be dragged to, in pixels.
const MIN_W_PX: f32 = 160.0;
const MIN_H_PX: f32 = 90.0;
/// The server rejects bounds that reach past the window even by a rounding
/// error, so dragged bounds stay this far inside its right and bottom edges.
const EDGE: f64 = 1e-6;
/// How far, as a fraction of the window, a drag moves before the layer's
/// new bounds go to the server. Like the split-divider drag, this keeps a
/// drag from sending a request for every pixel.
const SEND_STEP: f64 = 0.004;
/// How long a released drag keeps drawing the layer where it was dropped,
/// so it doesn't jump back for a frame before the server's view arrives.
const SETTLE: Duration = Duration::from_millis(400);

fn fail(code: &str, message: impl Into<String>) -> Outcome {
    Err((code.to_string(), message.into()))
}

fn done() -> Outcome {
    Ok(Value::Null)
}

/// A floating layer's title bar or resize corner, being dragged.
#[derive(Clone, Debug)]
pub(crate) struct LayerDrag {
    layer_id: String,
    /// Resizing from the bottom-right corner, or else moving.
    resize: bool,
    /// Where the mouse went down, in window pixels.
    start: Point<Pixels>,
    /// The layer's bounds when the drag started.
    from: [f64; 4],
    /// Where the layer is drawn during the drag.
    now: [f64; 4],
    /// The bounds last sent to the server.
    sent: [f64; 4],
    started: Instant,
    released: bool,
}

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    _args: &Value,
    target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    Some(match action {
        "pane.float" => toggle_float(ws, target, cx),
        "pane.new_floating" => new_floating(ws, cx),
        "pane.minimize_floating" => minimize_one(ws, cx),
        "pane.minimize_all_floating" => minimize_all(ws, cx),
        _ => return None,
    })
}

// Geometry. Bounds and rectangles are `[x, y, w, h]` fractions of the window.

fn wide(b: [f32; 4]) -> [f64; 4] {
    b.map(f64::from)
}

/// Bounds kept inside the window and at least the minimum size.
fn clamp_bounds(b: [f64; 4], min_w: f64, min_h: f64) -> [f64; 4] {
    let max = 1.0 - EDGE;
    let w = b[2].max(min_w).min(max);
    let h = b[3].max(min_h).min(max);
    let x = b[0].clamp(0.0, max - w);
    let y = b[1].clamp(0.0, max - h);
    [x, y, w, h]
}

/// A layer's bounds after its title bar is dragged by `dx`, `dy`.
fn moved(from: [f64; 4], dx: f64, dy: f64) -> [f64; 4] {
    // Keep the size: a move never shrinks the layer against an edge.
    clamp_bounds([from[0] + dx, from[1] + dy, from[2], from[3]], 0.0, 0.0)
}

/// A layer's bounds after its bottom-right corner is dragged by `dx`, `dy`.
/// The top-left corner stays put.
fn resized(from: [f64; 4], dx: f64, dy: f64, min_w: f64, min_h: f64) -> [f64; 4] {
    let max = 1.0 - EDGE;
    let (x, y) = (from[0].clamp(0.0, max), from[1].clamp(0.0, max));
    let w = (from[2] + dx).min(max - x).max(min_w.min(max - x));
    let h = (from[3] + dy).min(max - y).max(min_h.min(max - y));
    [x, y, w, h]
}

/// Whether bounds have moved far enough from the last sent ones to send.
fn worth_sending(now: [f64; 4], sent: [f64; 4]) -> bool {
    now.iter().zip(sent).any(|(a, b)| (a - b).abs() >= SEND_STEP)
}

/// A block's rectangle inside its layer, as fractions of the layer, from its
/// rectangle and the layer's bounds as fractions of the window.
fn inner_rect(rect: [f32; 4], bounds: [f32; 4]) -> [f32; 4] {
    if bounds[2] <= 0.0 || bounds[3] <= 0.0 {
        return [0.0, 0.0, 1.0, 1.0];
    }
    [
        (rect[0] - bounds[0]) / bounds[2],
        (rect[1] - bounds[1]) / bounds[3],
        rect[2] / bounds[2],
        rect[3] / bounds[3],
    ]
}

/// Where a new floating layer goes: stepped down and right of the ones
/// already showing, so they don't hide each other exactly.
fn cascade(showing: usize) -> [f64; 4] {
    let step = 0.03 * (showing % 5) as f64;
    [0.15 + step, 0.12 + step, 0.7, 0.7]
}

// Reading the view.

fn floating(win: &RexWindow) -> impl DoubleEndedIterator<Item = &Layer> {
    win.layers.iter().filter(|l| l.is_floating())
}

fn holds(layer: &Layer, block: &str) -> bool {
    layer.blocks.iter().any(|b| b.block_id.as_deref() == Some(block))
}

fn find_layer<'a>(win: &'a RexWindow, layer_id: &str) -> Option<&'a Layer> {
    win.layers.iter().find(|l| l.layer_id.as_deref() == Some(layer_id))
}

/// The floating layer drawn in front of the others.
fn front_layer(win: &RexWindow) -> Option<&str> {
    floating(win).rev().find(|l| !l.is_minimized()).and_then(|l| l.layer_id.as_deref())
}

/// The window holding a block, or the active window when no block is named.
fn window_of<'a>(view: &'a SessionView, block: Option<&str>) -> Option<&'a RexWindow> {
    match block {
        Some(b) => view.windows.iter().find(|w| w.layers.iter().any(|l| holds(l, b))),
        None => active_window(view),
    }
}

/// The block a layer shows in its title: the window's focused block when
/// the layer holds it, or else the layer's first block.
fn title_block<'a>(win: &'a RexWindow, layer: &'a Layer) -> Option<&'a str> {
    win.focused_block_id
        .as_deref()
        .filter(|b| holds(layer, b))
        .or_else(|| layer.blocks.iter().find_map(|b| b.block_id.as_deref()))
}

fn layer_title(ws: &Workspace, win: &RexWindow, layer: &Layer, cx: &gpui::App) -> String {
    let Some(block) = title_block(win, layer) else { return "Floating".into() };
    let from_pane = ws.store.read(cx).panes.get(block).map(|p| p.read(cx).title.clone()).filter(|t| !t.is_empty());
    let label = || layer.blocks.iter().find(|b| b.block_id.as_deref() == Some(block)).map(|b| b.label.clone());
    from_pane.or_else(label).filter(|t| !t.is_empty()).unwrap_or_else(|| "Floating".into())
}

/// What `pane.minimize_floating` does: the layer to change, and whether to
/// minimize it (or else restore it).
///
/// It minimizes the floating layer that has focus. Otherwise it brings back
/// the front-most minimized layer, and with none minimized it minimizes the
/// front-most showing one, so pressing it again undoes it.
fn minimize_choice(win: &RexWindow) -> Option<(String, bool)> {
    let focused = win.focused_block_id.as_deref();
    let showing = |l: &&Layer| !l.is_minimized();
    if let Some(l) = floating(win).filter(showing).find(|l| focused.is_some_and(|b| holds(l, b))) {
        return Some((l.layer_id.clone()?, true));
    }
    if let Some(l) = floating(win).rev().find(|l| l.is_minimized()) {
        return Some((l.layer_id.clone()?, false));
    }
    let l = floating(win).rev().find(showing)?;
    Some((l.layer_id.clone()?, true))
}

/// Whether minimizing a layer would leave its window with no pane in sight,
/// which the server refuses.
fn hides_last(win: &RexWindow, layer_id: &str) -> bool {
    tiled_leaves(win).is_empty()
        && floating(win).filter(|l| !l.is_minimized()).all(|l| l.layer_id.as_deref() == Some(layer_id))
}

/// What `pane.minimize_all_floating` does: each layer to change, and whether
/// to minimize it.
///
/// It minimizes every showing floating layer, or brings them all back when
/// there is none left to minimize. A window must keep a pane in sight, so
/// when its layout is empty the front-most layer stays.
fn minimize_all_choice(win: &RexWindow) -> Vec<(String, bool)> {
    let keep = if tiled_leaves(win).is_empty() { front_layer(win) } else { None };
    let minimize: Vec<(String, bool)> = floating(win)
        .filter(|l| !l.is_minimized() && l.layer_id.as_deref() != keep)
        .filter_map(|l| Some((l.layer_id.clone()?, true)))
        .collect();
    if !minimize.is_empty() {
        return minimize;
    }
    floating(win).filter(|l| l.is_minimized()).filter_map(|l| Some((l.layer_id.clone()?, false))).collect()
}

/// The `session.move_block` payloads that dock floating blocks into the
/// window's layout: the first beside the largest tiled pane, split along its
/// longer side, and each after that beside the one before it. `aspect` is
/// the window's width over its height.
fn dock_moves(win: &RexWindow, blocks: &[String], aspect: f32) -> Result<Vec<Value>, String> {
    let anchor = tiled_leaves(win)
        .into_iter()
        .filter(|(id, _)| !blocks.contains(id))
        .max_by(|(_, a), (_, b)| (a[2] * a[3]).total_cmp(&(b[2] * b[3])));
    let Some((mut anchor, r)) = anchor else { return Err("there is no tiled pane to dock beside".into()) };
    let direction = if r[2] * aspect >= r[3] { "horizontal" } else { "vertical" };
    let mut moves = Vec::new();
    for block in blocks {
        moves.push(json!({"block_id": block, "anchor_block_id": anchor, "direction": direction, "side": "after"}));
        anchor = block.clone();
    }
    Ok(moves)
}

// Actions.

fn aspect(ws: &Workspace) -> f32 {
    let size = ws.area.size;
    if size.width > px(0.0) && size.height > px(0.0) { size.width / size.height } else { 1.6 }
}

fn active(ws: &Workspace, cx: &gpui::App) -> Option<RexWindow> {
    ws.view(cx).and_then(|v| active_window(&v).cloned())
}

/// Floats the block, or docks it back into the layout if it floats already.
fn toggle_float(ws: &mut Workspace, target: &Target, cx: &mut Context<Workspace>) -> Outcome {
    let Some(view) = ws.view(cx) else { return fail("conflict", "no session is open") };
    let Some(win) = window_of(&view, target.block_id.as_deref()) else {
        return match &target.block_id {
            Some(b) => fail("not_found", format!("{b} is not in a window of this session")),
            None => done(),
        };
    };
    let Some(block) = target.block_id.clone().or_else(|| win.focused_block_id.clone()) else {
        return fail("conflict", "no pane is focused");
    };
    if floating(win).any(|l| holds(l, &block)) {
        return dock(ws, win, vec![block], cx);
    }
    let showing = floating(win).filter(|l| !l.is_minimized()).count();
    ws.session(cx, "session.float_block", json!({"block_id": block, "bounds": cascade(showing)}))
}

fn dock(ws: &mut Workspace, win: &RexWindow, blocks: Vec<String>, cx: &mut Context<Workspace>) -> Outcome {
    let moves = match dock_moves(win, &blocks, aspect(ws)) {
        Ok(moves) => moves,
        Err(why) => return fail("conflict", why),
    };
    for m in moves {
        ws.session(cx, "session.move_block", m)?;
    }
    done()
}

fn new_floating(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Outcome {
    let showing = active(ws, cx).map(|w| floating(&w).filter(|l| !l.is_minimized()).count()).unwrap_or(0);
    let layout = crate::store::shell_layout_in(ws.focused_cwd(cx));
    ws.session(cx, "session.new_layer", json!({"bounds": cascade(showing), "layout": layout, "focus": true}))
}

fn set_state(ws: &mut Workspace, layer_id: &str, minimize: bool, focus: bool, cx: &mut Context<Workspace>) -> Outcome {
    let mut payload = json!({"layer_id": layer_id, "state": if minimize { "minimized" } else { "normal" }});
    if focus && !minimize {
        // Also raises the layer and focuses its block, in one change.
        payload["focus"] = json!(true);
    }
    ws.session(cx, "session.set_layer_state", payload)
}

/// Minimizes a layer, unless it holds the last panes its window shows. The
/// server would refuse, naming the layer's ID, so say why in plain words.
fn minimize(ws: &mut Workspace, layer_id: &str, cx: &mut Context<Workspace>) -> Outcome {
    if active(ws, cx).is_some_and(|w| hides_last(&w, layer_id)) {
        return fail("conflict", "the window's last visible pane can't be minimized");
    }
    set_state(ws, layer_id, true, false, cx)
}

fn minimize_one(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Outcome {
    let Some(win) = active(ws, cx) else { return done() };
    match minimize_choice(&win) {
        Some((id, true)) => minimize(ws, &id, cx),
        Some((id, false)) => set_state(ws, &id, false, true, cx),
        None => done(),
    }
}

fn minimize_all(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Outcome {
    let Some(win) = active(ws, cx) else { return done() };
    let changes = minimize_all_choice(&win);
    let last = changes.len().saturating_sub(1);
    for (i, (id, minimize)) in changes.into_iter().enumerate() {
        // Bringing them back focuses the front one.
        set_state(ws, &id, minimize, i == last, cx)?;
    }
    done()
}

/// Brings a layer to the front and focuses its block, as a click on its
/// title does, unless it is already in front with focus.
fn activate(ws: &mut Workspace, layer_id: &str, cx: &mut Context<Workspace>) {
    let Some(win) = active(ws, cx) else { return };
    let in_front = front_layer(&win) == Some(layer_id);
    let has_focus = find_layer(&win, layer_id).zip(win.focused_block_id.as_deref()).is_some_and(|(l, b)| holds(l, b));
    if !(in_front && has_focus) {
        set_state(ws, layer_id, false, true, cx).ok();
    }
}

/// Brings a layer to the front when a click lands on it. The pane clicked
/// takes focus itself.
fn raise(ws: &mut Workspace, layer_id: &str, cx: &mut Context<Workspace>) {
    let Some(win) = active(ws, cx) else { return };
    if front_layer(&win) != Some(layer_id) {
        ws.session(cx, "session.raise_layer", json!({"layer_id": layer_id})).ok();
    }
}

fn dock_layer(ws: &mut Workspace, layer_id: &str, cx: &mut Context<Workspace>) {
    let Some(win) = active(ws, cx) else { return };
    let Some(layer) = find_layer(&win, layer_id) else { return };
    let blocks: Vec<String> = layer.blocks.iter().filter_map(|b| b.block_id.clone()).collect();
    let outcome = dock(ws, &win, blocks, cx);
    report(ws, outcome, cx);
}

fn report(ws: &mut Workspace, outcome: Outcome, cx: &mut Context<Workspace>) {
    if let Err((_, message)) = outcome {
        ws.store.update(cx, |s, cx| {
            s.error = Some(message);
            cx.notify();
        });
    }
}

// Dragging.

fn start_drag(ws: &mut Workspace, layer_id: &str, resize: bool, at: Point<Pixels>, bounds: [f64; 4]) {
    ws.layer_drag = Some(LayerDrag {
        layer_id: layer_id.to_string(),
        resize,
        start: at,
        from: bounds,
        now: bounds,
        sent: bounds,
        started: Instant::now(),
        released: false,
    });
}

fn send_bounds(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(drag) = &mut ws.layer_drag else { return };
    drag.sent = drag.now;
    let payload = json!({"layer_id": drag.layer_id, "bounds": drag.now});
    ws.session(cx, "session.resize_layer", payload).ok();
}

fn drag_move(ws: &mut Workspace, ev: &MouseMoveEvent, cx: &mut Context<Workspace>) {
    let area = ws.area.size;
    let Some(drag) = &mut ws.layer_drag else { return };
    if drag.released {
        return;
    }
    if ev.pressed_button != Some(MouseButton::Left) {
        // The button came up where this window didn't see it.
        return drag_end(ws, cx);
    }
    if area.width <= px(0.0) || area.height <= px(0.0) {
        return;
    }
    let dx = f64::from((ev.position.x - drag.start.x) / area.width);
    let dy = f64::from((ev.position.y - drag.start.y) / area.height);
    drag.now = if drag.resize {
        let (min_w, min_h) = (f64::from(px(MIN_W_PX) / area.width), f64::from(px(MIN_H_PX) / area.height));
        resized(drag.from, dx, dy, min_w, min_h)
    } else {
        moved(drag.from, dx, dy)
    };
    if worth_sending(drag.now, drag.sent) {
        send_bounds(ws, cx);
    }
    cx.notify();
}

fn drag_end(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(drag) = &mut ws.layer_drag else { return };
    if drag.released {
        return;
    }
    drag.released = true;
    let started = drag.started;
    if drag.now != drag.sent {
        send_bounds(ws, cx);
    }
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(SETTLE).await;
        this.update(cx, |ws, cx| {
            let Some(drag) = ws.layer_drag.as_ref().filter(|d| d.released && d.started == started) else { return };
            let (id, now) = (drag.layer_id.clone(), drag.now);
            ws.layer_drag = None;
            // Resizes don't run in order on the server, so an earlier one can
            // land after the last. If the layer isn't where it was dropped,
            // send its bounds once more.
            let server = active(ws, cx).and_then(|w| find_layer(&w, &id).map(|l| wide(l.bounds)));
            if server.is_some_and(|s| s.iter().zip(now).any(|(a, b)| (a - b).abs() > 1e-4)) {
                ws.session(cx, "session.resize_layer", json!({"layer_id": id, "bounds": now})).ok();
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}

// Drawing.

struct Colors {
    bg: Hsla,
    bar: Hsla,
    border: Hsla,
    edge: Hsla,
    text: Hsla,
    dim: Hsla,
    accent: Hsla,
    danger: Hsla,
}

fn colors() -> Colors {
    Colors {
        bg: rgb(0x16161e).into(),
        bar: rgb(0x1f2335).into(),
        border: rgb(0x2a2c3d).into(),
        edge: rgb(0x3b3f5c).into(),
        text: rgb(0xc0caf5).into(),
        dim: rgb(0x6b7394).into(),
        accent: rgb(0x7aa2f7).into(),
        danger: rgb(0xf7768e).into(),
    }
}

/// Every showing floating layer of a window, back to front, to draw over
/// its tiled layout.
pub(crate) fn render_floating(ws: &Workspace, win: &RexWindow, window: &Window, cx: &mut Context<Workspace>) -> Vec<AnyElement> {
    if win.zoomed_block_id.is_some() {
        // A zoomed block is drawn alone, over the whole window.
        return Vec::new();
    }
    let mut out: Vec<AnyElement> = floating(win)
        .filter(|l| !l.is_minimized())
        .enumerate()
        .filter_map(|(i, layer)| render_layer(ws, win, layer, i, window, cx))
        .collect();
    if ws.layer_drag.as_ref().is_some_and(|d| !d.released) {
        out.push(drag_listener(cx).into_any_element());
    }
    out
}

/// Follows the mouse anywhere in the window while a layer is dragged, not
/// just over the layer, which the mouse can outrun.
fn drag_listener(cx: &mut Context<Workspace>) -> impl IntoElement {
    let this = cx.entity();
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            let moves = this.clone();
            window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture {
                    let dragging = moves.update(cx, |ws, cx| {
                        drag_move(ws, ev, cx);
                        ws.layer_drag.as_ref().is_some_and(|d| !d.released)
                    });
                    if dragging {
                        // Keep the panes the mouse passes over from seeing the
                        // drag: a program tracking the mouse would get motion.
                        cx.stop_propagation();
                    }
                }
            });
            window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && ev.button == MouseButton::Left {
                    this.update(cx, |ws, cx| drag_end(ws, cx));
                }
            });
        },
    )
    .absolute()
    .size_full()
}

fn render_layer(
    ws: &Workspace,
    win: &RexWindow,
    layer: &Layer,
    index: usize,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let c = colors();
    let id = layer.layer_id.clone()?;
    let layout = layer.layout.as_ref()?;
    let drag = ws.layer_drag.as_ref().filter(|d| d.layer_id == id);
    let bounds = drag.map(|d| d.now).unwrap_or_else(|| wide(layer.bounds));
    let focused_here = win.focused_block_id.as_deref().is_some_and(|b| holds(layer, b));
    let store = ws.store.read(cx);

    let mut leaves = Vec::new();
    layout.leaves(&mut leaves);
    let mut content = div().relative().flex_1().w_full().overflow_hidden();
    for (block_id, r) in leaves {
        let Some(pane) = store.panes.get(&block_id) else { continue };
        let r = inner_rect(r, layer.bounds);
        let focused = win.focused_block_id.as_deref() == Some(block_id.as_str()) && pane.read(cx).focus.is_focused(window);
        // A lone block needs no border of its own inside the layer's.
        let border = match (focused, layer.blocks.len() > 1) {
            (_, false) => c.bg,
            (true, true) => c.accent,
            (false, true) => c.border,
        };
        content = content.child(
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

    let title = layer_title(ws, win, layer, cx);
    let button = |name: &'static str, glyph: &'static str, hover: Hsla| {
        div()
            .id(name)
            .px_1()
            .cursor_pointer()
            .text_color(c.dim)
            .hover(move |s| s.text_color(hover))
            .child(glyph)
    };
    let (min_id, dock_id, close_id, bar_id, grip_id, raise_id) =
        (id.clone(), id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    let bar = div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .h(px(TITLE_H))
        .pl_2()
        .pr_1()
        .bg(c.bar)
        .border_b_1()
        .border_color(c.border)
        .text_size(px(11.0))
        .text_color(if focused_here { c.text } else { c.dim })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                start_drag(ws, &bar_id, false, ev.position, bounds);
                activate(ws, &bar_id, cx);
                cx.notify();
            }),
        )
        // A quick click can come up before the window-wide drag listener is
        // drawn, so the bar ends the drag too.
        .on_mouse_up(MouseButton::Left, cx.listener(|ws, _: &MouseUpEvent, _, cx| drag_end(ws, cx)))
        .child(div().flex_1().min_w_0().truncate().child(SharedString::from(title)))
        .child(button("layer-minimize", "–", c.text).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let outcome = minimize(ws, &min_id, cx);
                report(ws, outcome, cx);
            }),
        ))
        .child(button("layer-dock", "⇲", c.text).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                dock_layer(ws, &dock_id, cx);
            }),
        ))
        .child(button("layer-close", "×", c.danger).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let outcome = ws.session(cx, "session.close_layer", json!({"layer_id": close_id}));
                report(ws, outcome, cx);
            }),
        ));

    let grip = div()
        .id("layer-resize")
        .absolute()
        .right_0()
        .bottom_0()
        .size(px(14.0))
        .flex()
        .items_end()
        .justify_end()
        .p(px(2.0))
        .occlude()
        .cursor(CursorStyle::ResizeUpLeftDownRight)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                start_drag(ws, &grip_id, true, ev.position, bounds);
                activate(ws, &grip_id, cx);
                cx.notify();
            }),
        )
        .on_mouse_up(MouseButton::Left, cx.listener(|ws, _: &MouseUpEvent, _, cx| drag_end(ws, cx)))
        .child(div().size(px(7.0)).border_r_2().border_b_2().border_color(c.dim));

    let shadow = BoxShadow {
        color: hsla(0.0, 0.0, 0.0, 0.55),
        offset: point(px(0.0), px(6.0)),
        blur_radius: px(18.0),
        spread_radius: px(0.0),
    };
    let el = div()
        .id(("floating-layer", index))
        .absolute()
        .left(relative(bounds[0] as f32))
        .top(relative(bounds[1] as f32))
        .w(relative(bounds[2] as f32))
        .h(relative(bounds[3] as f32))
        .flex()
        .flex_col()
        .bg(c.bg)
        .border_1()
        .border_color(if focused_here { c.accent } else { c.edge })
        .shadow(vec![shadow])
        // Clicks stop here, not on the panes underneath.
        .occlude()
        .on_any_mouse_down(cx.listener(move |ws, _: &MouseDownEvent, _, cx| raise(ws, &raise_id, cx)))
        .child(bar)
        .child(content)
        .child(grip);
    Some(el.into_any_element())
}

/// Chips for the active window's minimized layers, for the right end of the
/// tab bar. Clicking one brings its layer back.
pub(crate) fn render_minimized(ws: &Workspace, view: &SessionView, cx: &mut Context<Workspace>) -> impl IntoElement {
    let c = colors();
    let mut chips = div().flex().flex_shrink_0().items_center().gap_1().ml_auto().px_2();
    let Some(win) = active_window(view) else { return chips };
    for (i, layer) in floating(win).filter(|l| l.is_minimized()).enumerate() {
        let Some(id) = layer.layer_id.clone() else { continue };
        let title = layer_title(ws, win, layer, cx);
        chips = chips.child(
            div()
                .id(("minimized-layer", i))
                .flex()
                .items_center()
                .h(px(20.0))
                .max_w(px(160.0))
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(c.edge)
                .bg(c.bar)
                .text_size(px(11.0))
                .text_color(c.dim)
                .cursor_pointer()
                .hover(move |s| s.text_color(c.text).border_color(c.accent))
                .on_click(cx.listener(move |ws, _: &gpui::ClickEvent, _, cx| {
                    let outcome = set_state(ws, &id, false, true, cx);
                    report(ws, outcome, cx);
                }))
                .child(div().min_w_0().truncate().child(SharedString::from(title))),
        );
    }
    chips
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(json: &str) -> RexWindow {
        serde_json::from_str(json).unwrap()
    }

    /// A window with a tiled block `t`, a floating layer `f1` holding `a`,
    /// and a floating layer `f2` holding `b`, in front of `f1`.
    fn two_floating(focused: &str, f1_state: &str, f2_state: &str) -> RexWindow {
        let state = |s: &str| if s.is_empty() { String::new() } else { format!(r#","state":"{s}""#) };
        win(&format!(
            r#"{{"window_id":"w","focused_block_id":"{focused}","layers":[
            {{"layer_id":"tiled","kind":"tiled","bounds":[0,0,1,1],"layout":{{"rect":[0,0,1,1],"block_id":"t"}},"blocks":[{{"block_id":"t"}}]}},
            {{"layer_id":"f1","kind":"floating","bounds":[0.1,0.1,0.5,0.5]{},"layout":{{"rect":[0.1,0.1,0.5,0.5],"block_id":"a"}},"blocks":[{{"block_id":"a"}}]}},
            {{"layer_id":"f2","kind":"floating","bounds":[0.2,0.2,0.5,0.5]{},"layout":{{"rect":[0.2,0.2,0.5,0.5],"block_id":"b"}},"blocks":[{{"block_id":"b"}}]}}]}}"#,
            state(f1_state),
            state(f2_state)
        ))
    }

    fn inside(b: [f64; 4]) -> bool {
        b[0] >= 0.0 && b[1] >= 0.0 && b[2] > 0.0 && b[3] > 0.0 && b[0] + b[2] <= 1.0 && b[1] + b[3] <= 1.0
    }

    #[test]
    fn moving_keeps_the_size_and_stays_inside() {
        let from = [0.2, 0.2, 0.5, 0.4];
        let b = moved(from, 0.1, -0.05);
        assert!((b[0] - 0.3).abs() < 1e-9 && (b[1] - 0.15).abs() < 1e-9);
        let b = moved(from, 0.9, 0.9);
        assert!(inside(b));
        assert_eq!((b[2], b[3]), (0.5, 0.4));
        assert!((b[0] + b[2] - 1.0).abs() < 1e-5);
        let b = moved(from, -2.0, -2.0);
        assert_eq!(b, [0.0, 0.0, 0.5, 0.4]);
    }

    #[test]
    fn resizing_keeps_the_corner_and_a_minimum() {
        let from = [0.2, 0.2, 0.5, 0.4];
        let b = resized(from, 0.1, 0.1, 0.1, 0.1);
        assert!((b[2] - 0.6).abs() < 1e-9 && (b[3] - 0.5).abs() < 1e-9);
        assert_eq!((b[0], b[1]), (0.2, 0.2));
        let b = resized(from, 5.0, 5.0, 0.1, 0.1);
        assert!(inside(b));
        let b = resized(from, -5.0, -5.0, 0.1, 0.15);
        assert_eq!((b[2], b[3]), (0.1, 0.15));
        // A minimum that doesn't fit gives way to the window's edge.
        let b = resized([0.95, 0.95, 0.04, 0.04], 0.0, 0.0, 0.2, 0.2);
        assert!(inside(b));
    }

    #[test]
    fn clamped_bounds_never_reach_past_the_edge() {
        // The server rejects 0.30000001 + 0.7, so stay strictly inside.
        let b = clamp_bounds([0.30000001, 0.0, 0.7, 1.0], 0.0, 0.0);
        assert!(inside(b) && b[0] + b[2] < 1.0 && b[1] + b[3] < 1.0);
        let b = clamp_bounds([0.5, 0.5, 2.0, 2.0], 0.0, 0.0);
        assert!(inside(b));
    }

    #[test]
    fn sends_only_after_a_step() {
        let a = [0.2, 0.2, 0.5, 0.4];
        assert!(!worth_sending([0.201, 0.2, 0.5, 0.4], a));
        assert!(worth_sending([0.2, 0.2, 0.5, 0.41], a));
    }

    #[test]
    fn maps_blocks_into_their_layer() {
        let r = inner_rect([0.1, 0.35, 0.5, 0.25], [0.1, 0.1, 0.5, 0.5]);
        assert_eq!(r, [0.0, 0.5, 1.0, 0.5]);
        assert_eq!(inner_rect([0.1, 0.1, 0.5, 0.5], [0.0, 0.0, 0.0, 0.0]), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn cascades_new_layers_inside_the_window() {
        for n in 0..12 {
            assert!(inside(cascade(n)));
        }
        assert_ne!(cascade(0), cascade(1));
    }

    #[test]
    fn finds_the_front_layer() {
        assert_eq!(front_layer(&two_floating("t", "", "")), Some("f2"));
        assert_eq!(front_layer(&two_floating("t", "", "minimized")), Some("f1"));
        assert_eq!(front_layer(&two_floating("t", "minimized", "minimized")), None);
    }

    #[test]
    fn minimize_toggles_the_focused_then_restores() {
        // Focus in a floating layer: minimize it.
        assert_eq!(minimize_choice(&two_floating("a", "", "")), Some(("f1".to_string(), true)));
        // Focus in the layout, one minimized: bring it back.
        assert_eq!(minimize_choice(&two_floating("t", "minimized", "")), Some(("f1".to_string(), false)));
        // Focus in the layout, none minimized: minimize the front one.
        assert_eq!(minimize_choice(&two_floating("t", "", "")), Some(("f2".to_string(), true)));
        // Restores the front-most minimized one first.
        assert_eq!(minimize_choice(&two_floating("t", "minimized", "minimized")), Some(("f2".to_string(), false)));
    }

    #[test]
    fn minimize_all_toggles() {
        let mut all = minimize_all_choice(&two_floating("t", "", "minimized"));
        all.sort();
        assert_eq!(all, vec![("f1".to_string(), true)]);
        let all = minimize_all_choice(&two_floating("t", "minimized", "minimized"));
        assert_eq!(all, vec![("f1".to_string(), false), ("f2".to_string(), false)]);
    }

    #[test]
    fn minimize_all_keeps_a_pane_in_sight() {
        let w = win(
            r#"{"window_id":"w","focused_block_id":"a","layers":[
            {"layer_id":"tiled","kind":"tiled","bounds":[0,0,1,1],"layout":null,"blocks":[]},
            {"layer_id":"f1","kind":"floating","bounds":[0.1,0.1,0.5,0.5],"layout":{"rect":[0.1,0.1,0.5,0.5],"block_id":"a"},"blocks":[{"block_id":"a"}]},
            {"layer_id":"f2","kind":"floating","bounds":[0.2,0.2,0.5,0.5],"layout":{"rect":[0.2,0.2,0.5,0.5],"block_id":"b"},"blocks":[{"block_id":"b"}]}]}"#,
        );
        assert_eq!(minimize_all_choice(&w), vec![("f1".to_string(), true)]);
        assert!(dock_moves(&w, &["a".to_string()], 1.6).is_err());
        // With only the kept one showing, pressing it again brings the rest back.
        let w = win(
            r#"{"window_id":"w","focused_block_id":"b","layers":[
            {"layer_id":"tiled","kind":"tiled","layout":null,"blocks":[]},
            {"layer_id":"f1","kind":"floating","state":"minimized","layout":{"rect":[0.1,0.1,0.5,0.5],"block_id":"a"},"blocks":[{"block_id":"a"}]},
            {"layer_id":"f2","kind":"floating","layout":{"rect":[0.2,0.2,0.5,0.5],"block_id":"b"},"blocks":[{"block_id":"b"}]}]}"#,
        );
        assert_eq!(minimize_all_choice(&w), vec![("f1".to_string(), false)]);
    }

    #[test]
    fn knows_when_a_layer_holds_the_last_visible_pane() {
        // A tiled pane stays in sight whatever floats.
        assert!(!hides_last(&two_floating("t", "", ""), "f2"));
        let w = win(
            r#"{"window_id":"w","layers":[
            {"layer_id":"tiled","kind":"tiled","layout":null,"blocks":[]},
            {"layer_id":"f1","kind":"floating","state":"minimized","layout":{"rect":[0.1,0.1,0.5,0.5],"block_id":"a"},"blocks":[{"block_id":"a"}]},
            {"layer_id":"f2","kind":"floating","layout":{"rect":[0.2,0.2,0.5,0.5],"block_id":"b"},"blocks":[{"block_id":"b"}]}]}"#,
        );
        assert!(hides_last(&w, "f2"));
        let w = win(
            r#"{"window_id":"w","layers":[
            {"layer_id":"tiled","kind":"tiled","layout":null,"blocks":[]},
            {"layer_id":"f1","kind":"floating","layout":{"rect":[0.1,0.1,0.5,0.5],"block_id":"a"},"blocks":[{"block_id":"a"}]},
            {"layer_id":"f2","kind":"floating","layout":{"rect":[0.2,0.2,0.5,0.5],"block_id":"b"},"blocks":[{"block_id":"b"}]}]}"#,
        );
        assert!(!hides_last(&w, "f2"));
    }

    #[test]
    fn docks_beside_the_largest_tiled_pane() {
        let w = win(
            r#"{"window_id":"w","layers":[
            {"layer_id":"tiled","kind":"tiled","layout":{"rect":[0,0,1,1],"split":{"direction":"horizontal","ratio":0.3,
              "before":{"rect":[0,0,0.3,1],"block_id":"small"},"after":{"rect":[0.3,0,0.7,1],"block_id":"big"}}},
             "blocks":[{"block_id":"small"},{"block_id":"big"}]},
            {"layer_id":"f","kind":"floating","bounds":[0.1,0.1,0.5,0.5],"layout":{"rect":[0.1,0.1,0.5,0.5],"block_id":"x"},"blocks":[{"block_id":"x"}]}]}"#,
        );
        let moves = dock_moves(&w, &["x".to_string(), "y".to_string()], 1.6).unwrap();
        assert_eq!(moves[0]["anchor_block_id"], "big");
        assert_eq!(moves[0]["direction"], "horizontal");
        assert_eq!(moves[1]["anchor_block_id"], "x");
        // In a tall window the same pane splits top and bottom.
        let moves = dock_moves(&w, &["x".to_string()], 0.5).unwrap();
        assert_eq!(moves[0]["direction"], "vertical");
    }

    #[test]
    fn titles_the_focused_block_or_the_first() {
        let w = two_floating("b", "", "");
        assert_eq!(title_block(&w, &w.layers[2]), Some("b"));
        assert_eq!(title_block(&w, &w.layers[1]), Some("a"));
    }
}
