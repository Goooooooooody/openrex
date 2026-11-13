//! Finds the end of the snapshot that starts a terminal stream.
//!
//! The snapshot is in Ghostty's snapshot format: a ten-byte envelope (the
//! magic `GHOSTSNP` and a `u16` version) followed by records. Each record has
//! a ten-byte header (`u16` tag, `u32` payload length, `u32` CRC32C) and its
//! payload. The record tagged `finish` ends the snapshot. Integers are
//! little-endian.
//!
//! This only frames the snapshot. Decoding it is the terminal engine's job.

const MAGIC: &[u8; 8] = b"GHOSTSNP";
const ENVELOPE_LEN: usize = 10;
const RECORD_HEADER_LEN: usize = 10;
const TAG_FINISH: u16 = 6;

#[derive(Debug, PartialEq, Eq)]
pub enum Framed {
    /// The snapshot isn't complete yet.
    Pending,
    /// The stream doesn't start with a snapshot. Every byte is output.
    NotASnapshot(Vec<u8>),
    /// The whole snapshot, and the stream bytes that followed it.
    Complete { snapshot: Vec<u8>, rest: Vec<u8> },
}

/// Collects stream bytes until the snapshot at their start is complete.
#[derive(Default)]
pub struct SnapshotFramer {
    buf: Vec<u8>,
    /// Offset of the next record header to read.
    next: usize,
}

impl SnapshotFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, input: &[u8]) -> Framed {
        self.buf.extend_from_slice(input);
        let n = self.buf.len().min(MAGIC.len());
        if self.buf[..n] != MAGIC[..n] {
            return Framed::NotASnapshot(std::mem::take(&mut self.buf));
        }
        if self.buf.len() < ENVELOPE_LEN {
            return Framed::Pending;
        }
        if self.next == 0 {
            self.next = ENVELOPE_LEN;
        }
        loop {
            let Some(header) = self.buf.get(self.next..self.next + RECORD_HEADER_LEN) else {
                return Framed::Pending;
            };
            let tag = u16::from_le_bytes([header[0], header[1]]);
            let len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
            let end = self.next + RECORD_HEADER_LEN + len;
            if self.buf.len() < end {
                return Framed::Pending;
            }
            self.next = end;
            if tag == TAG_FINISH {
                let rest = self.buf.split_off(end);
                return Framed::Complete { snapshot: std::mem::take(&mut self.buf), rest };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tag: u16, payload: &[u8]) -> Vec<u8> {
        let mut r = tag.to_le_bytes().to_vec();
        r.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        r.extend_from_slice(&[0; 4]);
        r.extend_from_slice(payload);
        r
    }

    fn sample() -> Vec<u8> {
        let mut s = b"GHOSTSNP\x01\x00".to_vec();
        s.extend(record(1, &[7; 300]));
        s.extend(record(5, &[]));
        s.extend(record(4, &[1, 2, 3]));
        s.extend(record(TAG_FINISH, &[]));
        s
    }

    #[test]
    fn frames_at_every_split_point() {
        let snap = sample();
        let mut stream = snap.clone();
        stream.extend_from_slice(b"live output");
        for cut in 0..=stream.len() {
            let mut f = SnapshotFramer::new();
            let first = f.push(&stream[..cut]);
            let done = if cut >= snap.len() { first } else {
                assert_eq!(first, Framed::Pending, "cut at {cut}");
                f.push(&stream[cut..])
            };
            let Framed::Complete { snapshot, rest } = done else {
                panic!("cut at {cut}: {done:?}");
            };
            assert_eq!(snapshot, snap);
            let mut all = rest;
            if cut >= snap.len() {
                all.extend_from_slice(&stream[cut..]);
            }
            assert_eq!(all, b"live output", "cut at {cut}");
        }
    }

    #[test]
    fn passes_through_plain_output() {
        let mut f = SnapshotFramer::new();
        assert_eq!(f.push(b"GHO"), Framed::Pending);
        assert_eq!(f.push(b"ST in the shell"), Framed::NotASnapshot(b"GHOST in the shell".to_vec()));
        let mut f = SnapshotFramer::new();
        assert_eq!(f.push(b"\x1b[2J"), Framed::NotASnapshot(b"\x1b[2J".to_vec()));
    }
}
