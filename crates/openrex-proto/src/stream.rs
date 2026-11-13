//! Splits a terminal stream into program output and Rex's own messages.
//!
//! Rex places its messages in the stream as APC sequences of the form
//! `ESC _ rex1;<nonce>;<json> ESC \`. One whose nonce doesn't match was
//! printed by a program, and is passed through as output.

use serde_json::Value;

const ESC: u8 = 0x1b;
/// The longest Rex message kept while waiting for its end. A theme with a
/// full 256-color palette is a few kilobytes.
const MAX_MESSAGE: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum RexMessage {
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        owner: Option<String>,
    },
    Theme(Value),
    ChildExited {
        exit_code: i64,
    },
    Unknown(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StreamItem {
    /// Bytes to feed to the terminal emulator.
    Vt(Vec<u8>),
    Rex(RexMessage),
}

pub struct StreamSplitter {
    prefix: Vec<u8>,
    /// Bytes from an ESC that may start a Rex message, held until more arrive.
    carry: Vec<u8>,
}

impl StreamSplitter {
    pub fn new(nonce: u64) -> Self {
        let mut prefix = vec![ESC, b'_'];
        prefix.extend_from_slice(format!("rex1;{nonce};").as_bytes());
        StreamSplitter { prefix, carry: Vec::new() }
    }

    /// Feeds stream bytes and returns what they complete, in order.
    pub fn push(&mut self, input: &[u8]) -> Vec<StreamItem> {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(input);

        let mut items = Vec::new();
        let mut vt: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let Some(off) = data[i..].iter().position(|&b| b == ESC) else {
                vt.extend_from_slice(&data[i..]);
                break;
            };
            vt.extend_from_slice(&data[i..i + off]);
            let start = i + off;
            let rest = &data[start..];

            let n = rest.len().min(self.prefix.len());
            if rest[..n] != self.prefix[..n] {
                // Not ours. Pass the ESC through and keep scanning.
                vt.push(ESC);
                i = start + 1;
                continue;
            }
            if rest.len() < self.prefix.len() {
                self.carry = rest.to_vec();
                break;
            }
            let body = &rest[self.prefix.len()..];
            match body.windows(2).position(|w| w == [ESC, b'\\']) {
                Some(end) => {
                    if !vt.is_empty() {
                        items.push(StreamItem::Vt(std::mem::take(&mut vt)));
                    }
                    items.push(StreamItem::Rex(parse_message(&body[..end])));
                    i = start + self.prefix.len() + end + 2;
                }
                None if rest.len() > MAX_MESSAGE => {
                    // Never terminated. Treat it as output rather than
                    // holding the stream forever.
                    vt.push(ESC);
                    i = start + 1;
                }
                None => {
                    self.carry = rest.to_vec();
                    break;
                }
            }
        }
        if !vt.is_empty() {
            items.push(StreamItem::Vt(vt));
        }
        items
    }
}

fn parse_message(json: &[u8]) -> RexMessage {
    let Ok(v) = serde_json::from_slice::<Value>(json) else {
        return RexMessage::Unknown(Value::Null);
    };
    match v["type"].as_str() {
        Some("resize") => RexMessage::Resize {
            cols: v["cols"].as_u64().unwrap_or(0) as u16,
            rows: v["rows"].as_u64().unwrap_or(0) as u16,
            cell_width_px: v["cell_width_px"].as_u64().unwrap_or(0) as u32,
            cell_height_px: v["cell_height_px"].as_u64().unwrap_or(0) as u32,
            owner: v["owner"].as_str().map(str::to_string),
        },
        Some("theme") => RexMessage::Theme(v),
        Some("child_exited") => RexMessage::ChildExited {
            exit_code: v["exit_code"].as_i64().unwrap_or(0),
        },
        _ => RexMessage::Unknown(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vt(s: &str) -> StreamItem {
        StreamItem::Vt(s.as_bytes().to_vec())
    }

    fn resize(cols: u16, rows: u16) -> StreamItem {
        StreamItem::Rex(RexMessage::Resize {
            cols,
            rows,
            cell_width_px: 0,
            cell_height_px: 0,
            owner: None,
        })
    }

    const MSG: &str = "\x1b_rex1;42;{\"type\":\"resize\",\"cols\":80,\"rows\":24}\x1b\\";

    #[test]
    fn splits_message_from_output() {
        let mut s = StreamSplitter::new(42);
        let input = format!("abc{MSG}def");
        assert_eq!(s.push(input.as_bytes()), vec![vt("abc"), resize(80, 24), vt("def")]);
    }

    #[test]
    fn handles_every_split_point() {
        let input = format!("ab\x1b[1mc{MSG}d\x1b]0;t\x07e");
        let bytes = input.as_bytes();
        for cut in 0..=bytes.len() {
            let mut s = StreamSplitter::new(42);
            let mut items = s.push(&bytes[..cut]);
            items.extend(s.push(&bytes[cut..]));
            let mut out = Vec::new();
            let mut msgs = 0;
            for item in items {
                match item {
                    StreamItem::Vt(b) => out.extend(b),
                    StreamItem::Rex(m) => {
                        assert_eq!(StreamItem::Rex(m), resize(80, 24));
                        msgs += 1;
                    }
                }
            }
            assert_eq!(msgs, 1, "cut at {cut}");
            assert_eq!(out, b"ab\x1b[1mcd\x1b]0;t\x07e", "cut at {cut}");
        }
    }

    #[test]
    fn passes_through_wrong_nonce() {
        let mut s = StreamSplitter::new(7);
        let items = s.push(MSG.as_bytes());
        assert_eq!(items, vec![vt(MSG)]);
    }

    #[test]
    fn byte_at_a_time() {
        let mut s = StreamSplitter::new(42);
        let input = format!("x{MSG}y");
        let mut out = Vec::new();
        let mut msgs = 0;
        for b in input.as_bytes() {
            for item in s.push(&[*b]) {
                match item {
                    StreamItem::Vt(b) => out.extend(b),
                    StreamItem::Rex(_) => msgs += 1,
                }
            }
        }
        assert_eq!((out.as_slice(), msgs), (b"xy".as_slice(), 1));
    }
}
