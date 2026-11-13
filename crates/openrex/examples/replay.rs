//! Replays a captured terminal stream through the engine and prints the
//! screen. `cargo run --example replay -- <stream-file> <nonce>`
use ghostty_vt::render::{CellIterator, RenderState, RowIterator};
use ghostty_vt::snapshot::Decoder;
use openrex_proto::{Framed, SnapshotFramer, StreamItem, StreamSplitter};

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("stream file");
    let nonce: u64 = std::env::args().nth(2).expect("nonce").parse()?;
    let bytes = std::fs::read(path)?;
    let mut framer = SnapshotFramer::new();
    let mut term = None;
    let mut live = Vec::new();
    // Feed in small pieces, as the network would.
    for chunk in bytes.chunks(97) {
        if term.is_some() {
            live.extend_from_slice(chunk);
            continue;
        }
        match framer.push(chunk) {
            Framed::Pending => {}
            Framed::NotASnapshot(b) => anyhow::bail!("no snapshot, {} bytes", b.len()),
            Framed::Complete { snapshot, rest } => {
                println!("snapshot: {} bytes", snapshot.len());
                let t = Decoder::from_slice(&snapshot).decode().map_err(|e| anyhow::anyhow!("decode: {e:?}"))?;
                println!("decoded: {}x{}", t.cols().unwrap(), t.rows().unwrap());
                term = Some(t);
                live.extend(rest);
            }
        }
    }
    let mut term = term.expect("snapshot never completed");
    let mut splitter = StreamSplitter::new(nonce);
    for item in splitter.push(&live) {
        match item {
            StreamItem::Vt(b) => term.vt_write(&b),
            StreamItem::Rex(m) => println!("rex: {m:?}"),
        }
    }
    for ev in term.events() {
        println!("event: {ev:?}");
    }
    let mut rs = RenderState::new().unwrap();
    let mut rows = RowIterator::new().unwrap();
    let mut cells = CellIterator::new().unwrap();
    let snap = rs.update(&term).unwrap();
    println!("cursor: {:?}", snap.cursor().unwrap());
    let mut it = rows.update(&snap).unwrap();
    let mut y = 0;
    while let Some(row) = it.next() {
        let mut line = String::new();
        let mut ci = cells.update(row).unwrap();
        while let Some(cell) = ci.next() {
            let before = line.len();
            cell.graphemes_utf8(&mut line).unwrap();
            if line.len() == before {
                line.push(' ');
            }
        }
        let line = line.trim_end();
        if !line.is_empty() {
            println!("{y:2}| {line}");
        }
        y += 1;
    }
    Ok(())
}
