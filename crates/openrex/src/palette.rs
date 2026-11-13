//! The command palette: a list to pick from, or a line to type into, shown
//! over the window. Every choice performs an action with arguments, so the
//! palette, key bindings, and other clients all go through one dispatcher.

use serde_json::{Value, json};

/// One row of the palette.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub label: String,
    /// Smaller text after the label, such as a session's tab count.
    pub detail: String,
    /// Text on the right, such as the action's keys.
    pub hint: String,
    /// More words to match the query against.
    pub keywords: String,
    pub action: String,
    pub args: Value,
}

impl Item {
    pub fn new(label: impl Into<String>, action: impl Into<String>, args: Value) -> Item {
        Item { label: label.into(), action: action.into(), args, ..Default::default() }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Item {
        self.detail = detail.into();
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Item {
        self.hint = hint.into();
        self
    }

    pub fn keywords(mut self, keywords: impl Into<String>) -> Item {
        self.keywords = keywords.into();
        self
    }
}

/// What typing does.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// The query filters `items`. Enter performs the selected one.
    Pick,
    /// The query is a value. Enter performs `action` with the query as the
    /// argument `arg`, merged into `args`. `items` are suggestions: Tab copies
    /// the selected one's label into the query, and Enter performs the
    /// selected one if the query is empty.
    Input { action: String, args: Value, arg: String },
}

/// What a suggestion list follows as the query changes.
#[derive(Clone, Debug, PartialEq)]
pub enum Suggest {
    None,
    /// Directories, and files too if `files`, below the typed path, listed
    /// by the terminal `block_id`.
    Paths { session_id: String, block_id: String, files: bool },
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub title: String,
    pub placeholder: String,
    pub query: String,
    pub kind: Kind,
    pub items: Vec<Item>,
    pub selected: usize,
    pub suggest: Suggest,
    /// What the palette shows when nothing matches.
    pub empty: String,
}

impl Palette {
    pub fn pick(title: impl Into<String>, items: Vec<Item>) -> Palette {
        Palette {
            title: title.into(),
            placeholder: "Type to filter".into(),
            query: String::new(),
            kind: Kind::Pick,
            items,
            selected: 0,
            suggest: Suggest::None,
            empty: "Nothing matches".into(),
        }
    }

    pub fn input(title: impl Into<String>, action: &str, args: Value, arg: &str, initial: &str) -> Palette {
        Palette {
            title: title.into(),
            placeholder: String::new(),
            query: initial.to_string(),
            kind: Kind::Input { action: action.into(), args, arg: arg.into() },
            items: Vec::new(),
            selected: 0,
            suggest: Suggest::None,
            empty: String::new(),
        }
    }

    pub fn placeholder(mut self, text: impl Into<String>) -> Palette {
        self.placeholder = text.into();
        self
    }

    pub fn empty(mut self, text: impl Into<String>) -> Palette {
        self.empty = text.into();
        self
    }

    pub fn is_input(&self) -> bool {
        matches!(self.kind, Kind::Input { .. })
    }

    /// The items that match the query, best first. Suggestions under an
    /// input are already filtered by whoever made them.
    pub fn visible(&self) -> Vec<&Item> {
        if self.is_input() {
            return self.items.iter().collect();
        }
        let words: Vec<String> = self.query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return self.items.iter().collect();
        }
        let mut scored: Vec<(usize, usize, &Item)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| score(item, &words).map(|s| (s, i, item)))
            .collect();
        scored.sort_by_key(|(s, i, _)| (*s, *i));
        scored.into_iter().map(|(_, _, item)| item).collect()
    }

    pub fn move_selection(&mut self, delta: isize) {
        let n = self.visible().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.selected = 0;
    }

    /// What Enter does: an action and its arguments, or nothing.
    pub fn submit(&self) -> Option<(String, Value)> {
        let visible = self.visible();
        match &self.kind {
            Kind::Pick => visible.get(self.selected).map(|item| (item.action.clone(), item.args.clone())),
            Kind::Input { action, args, arg } => {
                if self.query.trim().is_empty() {
                    if let Some(item) = visible.get(self.selected) {
                        return Some((item.action.clone(), item.args.clone()));
                    }
                }
                let mut args = if args.is_object() { args.clone() } else { json!({}) };
                args[arg.as_str()] = json!(self.query);
                Some((action.clone(), args))
            }
        }
    }

    /// Completes the query from the selected suggestion.
    pub fn complete(&mut self) {
        if let Some(item) = self.visible().get(self.selected) {
            let label = item.label.clone();
            self.set_query(label);
        }
    }
}

/// How well an item matches every word of the query, lower is better, or
/// `None` if a word is missing.
fn score(item: &Item, words: &[String]) -> Option<usize> {
    let label = item.label.to_lowercase();
    let rest = format!("{} {}", item.detail, item.keywords).to_lowercase();
    let mut total = 0;
    for w in words {
        total += if label.starts_with(w.as_str()) {
            0
        } else if let Some(i) = label.find(w.as_str()) {
            // A word that starts a word of the label beats one inside it.
            if label[..i].ends_with([' ', '-', '_', '/', '.']) { 1 } else { 2 + i }
        } else if rest.contains(w.as_str()) {
            50
        } else if subsequence(&label, w) {
            100
        } else {
            return None;
        };
    }
    Some(total)
}

fn subsequence(hay: &str, needle: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Removes the last word of `s`, as Ctrl+Backspace does.
pub fn delete_word(s: &mut String) {
    let trimmed = s.trim_end_matches(|c: char| c == ' ' || c == '/').len();
    s.truncate(trimmed);
    let cut = s.rfind([' ', '/']).map(|i| i + 1).unwrap_or(0);
    s.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Item> {
        vec![
            Item::new("Split Pane Right", "pane.split.right", Value::Null).keywords("pane split terminal"),
            Item::new("New Tab", "client.tab.new", Value::Null).keywords("create tab window"),
            Item::new("Rename Session", "session.rename", Value::Null),
            Item::new("Toggle Sidebar", "client.sidebar.toggle", Value::Null),
        ]
    }

    #[test]
    fn filters_and_ranks_by_label_then_keywords() {
        let mut p = Palette::pick("Commands", items());
        assert_eq!(p.visible().len(), 4);
        p.set_query("tab".into());
        assert_eq!(p.visible()[0].action, "client.tab.new");
        p.set_query("split right".into());
        assert_eq!(p.visible().len(), 1);
        p.set_query("window".into());
        assert_eq!(p.visible()[0].action, "client.tab.new");
        p.set_query("rnmses".into());
        assert_eq!(p.visible()[0].action, "session.rename");
        p.set_query("zzz".into());
        assert!(p.visible().is_empty());
        assert_eq!(p.submit(), None);
    }

    #[test]
    fn input_submits_the_query_as_an_argument() {
        let mut p = Palette::input("Rename Tab", "client.tab.rename", json!({"window": "w"}), "label", "old");
        p.set_query("build".into());
        assert_eq!(p.submit(), Some(("client.tab.rename".into(), json!({"window": "w", "label": "build"}))));
    }

    #[test]
    fn selection_wraps_and_words_delete() {
        let mut p = Palette::pick("Commands", items());
        p.move_selection(-1);
        assert_eq!(p.selected, 3);
        p.move_selection(1);
        assert_eq!(p.selected, 0);
        let mut s = "cd /usr/local/bin".to_string();
        delete_word(&mut s);
        assert_eq!(s, "cd /usr/local/");
        delete_word(&mut s);
        assert_eq!(s, "cd /usr/");
    }
}
