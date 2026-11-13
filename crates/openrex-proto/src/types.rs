//! Shapes of the results and events the client reads.
//!
//! Every field the server may leave out has a default, and unknown fields are
//! ignored, because newer servers add them.

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SessionView {
    pub revision: u64,
    pub session_id: Option<String>,
    pub label: String,
    pub active_window_id: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Window {
    pub window_id: Option<String>,
    pub label: String,
    pub focused_block_id: Option<String>,
    pub zoomed_block_id: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub layers: Vec<Layer>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Layer {
    pub layer_id: Option<String>,
    pub kind: String,
    /// `[x, y, w, h]` as fractions of the window.
    pub bounds: [f32; 4],
    pub state: Option<String>,
    pub layout: Option<LayoutNode>,
    #[serde(deserialize_with = "null_as_default")]
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct LayoutNode {
    /// `[x, y, w, h]` as fractions of the window.
    pub rect: [f32; 4],
    pub block_id: Option<String>,
    pub split: Option<Box<Split>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Split {
    pub split_id: Option<String>,
    pub direction: String,
    pub ratio: f32,
    pub before: LayoutNode,
    pub after: LayoutNode,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Block {
    pub block_id: Option<String>,
    pub label: String,
    pub creator_name: String,
    pub flavor: String,
    pub host_id: String,
    pub owner: Option<String>,
}

impl LayoutNode {
    /// Every block under this node with its rectangle, in layout order.
    pub fn leaves(&self, out: &mut Vec<(String, [f32; 4])>) {
        if let Some(split) = &self.split {
            split.before.leaves(out);
            split.after.leaves(out);
        } else if let Some(id) = &self.block_id {
            out.push((id.clone(), self.rect));
        }
    }

    /// Every split under this node with its own rectangle.
    pub fn splits<'a>(&'a self, out: &mut Vec<(&'a Split, [f32; 4])>) {
        if let Some(split) = &self.split {
            out.push((split, self.rect));
            split.before.splits(out);
            split.after.splits(out);
        }
    }
}

impl Layer {
    pub fn is_floating(&self) -> bool {
        self.kind != "tiled"
    }

    pub fn is_minimized(&self) -> bool {
        self.state.as_deref() == Some("minimized")
    }
}

fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_view_with_nulls_and_unknown_fields() {
        let v: SessionView = serde_json::from_str(
            r#"{"revision":3,"session_id":"session:a","label":"work","future_field":1,
                "active_window_id":"window:w","windows":[{"window_id":"window:w","layers":[
                {"layer_id":"layer:l","kind":"tiled","bounds":[0,0,1,1],"blocks":null,
                 "layout":{"rect":[0,0,1,1],"split":{"split_id":"split:s","direction":"horizontal",
                 "ratio":0.5,"before":{"rect":[0,0,0.5,1],"block_id":"block:a"},
                 "after":{"rect":[0.5,0,0.5,1],"block_id":"block:b"}}}}]}]}"#,
        )
        .unwrap();
        let mut leaves = Vec::new();
        v.windows[0].layers[0].layout.as_ref().unwrap().leaves(&mut leaves);
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[1], ("block:b".to_string(), [0.5, 0.0, 0.5, 1.0]));
    }
}
