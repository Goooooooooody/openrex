//! Moving files between this machine and a terminal's: pane.upload (and dropping files on a pane) and pane.download.

use gpui::{Context, Window};
use serde_json::Value;

use crate::actions::{Outcome, Target};
use crate::workspace::Workspace;

/// Performs this module's actions. `None` for an action it doesn't own.
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
