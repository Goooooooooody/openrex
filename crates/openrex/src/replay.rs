//! Replays scripted key presses into the window, for testing the client
//! without a keyboard: `OPENREX_REPLAY` holds steps separated by spaces.
//!
//! - A keystroke in gpui's syntax, such as `a`, `enter`, `space`, or
//!   `ctrl-shift-d`. `shift-a` types a capital.
//! - `wait@MS`: pause.
//!
//! The keys go through the same dispatch as real ones, so key bindings,
//! focus, and the panes' handlers all take part. gpui offers no public way
//! to do the same for the mouse.

use std::time::Duration;

use gpui::{AsyncWindowContext, Keystroke, Modifiers};

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Key(Keystroke),
    Wait(u64),
}

pub fn parse(script: &str) -> Result<Vec<Step>, String> {
    script
        .split_whitespace()
        .map(|token| {
            let bad = || format!("unknown replay step {token:?}");
            match token.split_once('@') {
                Some(("wait", ms)) => ms.parse().map(Step::Wait).map_err(|_| bad()),
                Some(_) => Err(bad()),
                None => Keystroke::parse(token).map(Step::Key).map_err(|_| bad()),
            }
        })
        .collect()
}

/// Performs the steps, a little apart, the way a person's input arrives.
pub async fn run(steps: Vec<Step>, cx: &mut AsyncWindowContext) {
    let executor = cx.background_executor().clone();
    for step in steps {
        match step {
            Step::Wait(ms) => executor.timer(Duration::from_millis(ms)).await,
            Step::Key(mut keystroke) => {
                // A typed capital carries its text, as the keyboard sends it.
                let shift_only = Modifiers { shift: true, ..Default::default() };
                if keystroke.modifiers == shift_only && keystroke.key.len() == 1 {
                    keystroke.key_char = Some(keystroke.key.to_uppercase());
                }
                if cx.update(|window, cx| window.dispatch_keystroke(keystroke, cx)).is_err() {
                    return;
                }
                executor.timer(Duration::from_millis(25)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_script() {
        let steps = parse("e ctrl-shift-d enter wait@250").unwrap();
        assert_eq!(steps.len(), 4);
        assert!(matches!(&steps[0], Step::Key(k) if k.key == "e"));
        assert!(matches!(&steps[1], Step::Key(k) if k.key == "d" && k.modifiers.control && k.modifiers.shift));
        assert!(matches!(&steps[2], Step::Key(k) if k.key == "enter"));
        assert_eq!(steps[3], Step::Wait(250));
        assert!(parse("wait@soon").is_err());
        assert!(parse("click@1,2").is_err());
    }
}
