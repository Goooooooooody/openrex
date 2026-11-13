//! Links in terminals: OSC 8 hyperlinks and URLs in the text, opened with Ctrl+click.

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
