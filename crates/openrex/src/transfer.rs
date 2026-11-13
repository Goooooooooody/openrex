//! Moving files between this machine and a terminal's: pane.upload (and dropping files on a pane) and pane.download.
//!
//! Both go through the terminal's block methods. `download_begin` opens a
//! file on the terminal's machine and `upload_begin` reserves one there; each
//! answers with a ticket for a `rex.data.bytes.v1` stream that carries the
//! file's bytes. Uploaded files arrive in the server's upload directory, and
//! once every file of a batch has arrived the terminal types their paths.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::future::{Either, select};
use gpui::{App, Context, ExternalPaths, PathPromptOptions, Task, Window};
use openrex_proto::control::Control;
use openrex_proto::data::{ByteStream, MAX_CHUNK, new_ref, random_uuid};
use openrex_proto::Endpoint;
use serde_json::{Value, json};

use crate::actions::{Outcome, Target, shell_quote};
use crate::store::{Store, focused_block};
use crate::workspace::Workspace;

/// Performs this module's actions. `None` for an action it doesn't own.
pub(crate) fn perform(
    ws: &mut Workspace,
    action: &str,
    args: &Value,
    target: &Target,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Outcome> {
    match action {
        "pane.download" => Some(download_action(ws, args, target, cx)),
        "pane.upload" => Some(upload_action(ws, args, target, cx)),
        _ => None,
    }
}

fn conflict(message: &str) -> Outcome {
    Err(("conflict".to_string(), message.to_string()))
}

/// The session and block an action applies to: the ones it names, or the
/// focused pane.
fn target_pane(ws: &Workspace, target: &Target, cx: &App) -> Option<(String, String)> {
    let store = ws.store.read(cx);
    let session = target.session_id.clone().or_else(|| store.current.clone())?;
    let block = match &target.block_id {
        Some(b) => b.clone(),
        None => focused_block(store.views.get(&session)?)?.to_string(),
    };
    Some((session, block))
}

fn download_action(ws: &mut Workspace, args: &Value, target: &Target, cx: &mut Context<Workspace>) -> Outcome {
    let path = args.get("path").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
    let Some(path) = path else {
        return ws.open_path_input("Download File", "pane.download", true, cx);
    };
    let Some((session, block)) = target_pane(ws, target, cx) else { return conflict("no pane is focused") };
    start_download(ws, session, block, path.to_string(), cx);
    Ok(Value::Null)
}

fn upload_action(ws: &mut Workspace, args: &Value, target: &Target, cx: &mut Context<Workspace>) -> Outcome {
    let Some((session, block)) = target_pane(ws, target, cx) else { return conflict("no pane is focused") };
    let paths: Vec<PathBuf> = args
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(expand_home)
        .collect();
    if !paths.is_empty() {
        start_upload(ws, session, block, paths, cx);
        return Ok(Value::Null);
    }
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Upload".into()),
    });
    cx.spawn(async move |this, cx| {
        let picked = picked.await;
        this.update(cx, |ws, cx| match picked {
            Ok(Ok(Some(paths))) if !paths.is_empty() => start_upload(ws, session, block, paths, cx),
            Ok(Err(e)) => show_error(&ws.store, format!("pane.upload: the file chooser failed: {e:#}"), cx),
            // Canceled.
            _ => {}
        })
        .ok();
    })
    .detach();
    Ok(Value::Null)
}

/// A listener for files dropped on the pane of `block_id`, which uploads
/// them to it.
pub(crate) fn drop_listener(
    block_id: String,
    cx: &Context<Workspace>,
) -> impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static {
    cx.listener(move |ws, paths: &ExternalPaths, _, cx| {
        let session = ws.store.read(cx).panes.get(&block_id).map(|p| p.read(cx).session_id.clone());
        if let Some(session) = session {
            start_upload(ws, session, block_id.clone(), paths.paths().to_vec(), cx);
        }
    })
}

fn show_error(store: &gpui::Entity<Store>, message: String, cx: &mut App) {
    store.update(cx, |s, cx| {
        s.notice = None;
        s.error = Some(message);
        cx.notify();
    });
}

fn show_notice(store: &gpui::Entity<Store>, message: String, cx: &mut App) {
    store.update(cx, |s, cx| {
        s.error = None;
        s.notice = Some(message);
        cx.notify();
    });
}

fn start_download(ws: &mut Workspace, session: String, block: String, path: String, cx: &mut Context<Workspace>) {
    let store = ws.store.clone();
    let (control, endpoint) = {
        let s = store.read(cx);
        (s.control(), s.endpoint.clone())
    };
    let Some(control) = control else { return show_error(&store, "pane.download: not connected".into(), cx) };
    let Some(dir) = downloads_dir() else { return show_error(&store, "pane.download: HOME is not set".into(), cx) };
    let name = last_component(&path);
    show_notice(&store, format!("Downloading {name}…"), cx);
    let progress = Arc::new(Progress::default());
    let job = cx.background_executor().spawn({
        let progress = progress.clone();
        async move { download(&control, &endpoint, &session, &block, &path, &dir, &progress).await }
    });
    watch(store, job, progress, format!("Downloading {name}"), cx, |store, result, cx| match result {
        Ok(got) => show_notice(store, format!("Downloaded {} to {}", got.name, tilde(&got.saved)), cx),
        Err(e) => show_error(store, format!("pane.download: {e:#}"), cx),
    });
}

/// What an upload did.
enum Sent {
    /// The files went to the server, which typed their paths.
    Uploaded(usize),
    /// The terminal sees this machine's files, so their paths were typed
    /// as they are.
    Local(String, usize),
}

pub(crate) fn start_upload(ws: &mut Workspace, session: String, block: String, paths: Vec<PathBuf>, cx: &mut Context<Workspace>) {
    let store = ws.store.clone();
    let files = match local_files(&paths) {
        Ok(files) => files,
        Err(e) => return show_error(&store, format!("pane.upload: {e:#}"), cx),
    };
    let (control, endpoint, host_id) = {
        let s = store.read(cx);
        (s.control(), s.endpoint.clone(), block_host_id(s, &session, &block))
    };
    let Some(control) = control else { return show_error(&store, "pane.upload: not connected".into(), cx) };
    let what = match files.as_slice() {
        [one] => one.name.clone(),
        many => format!("{} files", many.len()),
    };
    show_notice(&store, format!("Uploading {what}…"), cx);
    let progress = Arc::new(Progress::default());
    let job = cx.background_executor().spawn({
        let (progress, block) = (progress.clone(), block.clone());
        async move {
            if matches!(endpoint, Endpoint::Unix(_)) && !host_id.is_empty() {
                let server = control.call("server.status", json!({})).await.ok();
                let server_host = server.as_ref().and_then(|v| v["host_id"].as_str());
                if sees_local_files(&endpoint, server_host, &host_id) {
                    return anyhow::Ok(Sent::Local(typed_paths(&files), files.len()));
                }
            }
            upload(&control, &endpoint, &session, &block, &files, &progress).await?;
            anyhow::Ok(Sent::Uploaded(files.len()))
        }
    });
    watch(store, job, progress, format!("Uploading {what}"), cx, move |store, result, cx| match result {
        Ok(Sent::Uploaded(n)) => {
            let what = if n == 1 { "the file, and the terminal typed its path".to_string() } else { format!("{n} files, and the terminal typed their paths") };
            show_notice(store, format!("Uploaded {what}"), cx);
        }
        Ok(Sent::Local(text, n)) => {
            paste(store, &block, text, cx);
            let what = if n == 1 { "its path was" } else { "their paths were" };
            show_notice(store, format!("The terminal is on this machine, so {what} typed without uploading"), cx);
        }
        Err(e) => show_error(store, format!("pane.upload: {e:#}"), cx),
    });
}

/// Types text into a terminal as a paste.
fn paste(store: &gpui::Entity<Store>, block: &str, text: String, cx: &mut App) {
    let s = store.read(cx);
    if let Some(pane) = s.panes.get(block).cloned() {
        pane.update(cx, |pane, _| pane.paste(text));
    } else if let (Some(control), Some(session)) = (s.control(), block_session(s, block)) {
        control.send_nowait(
            "block.event",
            json!({"session_id": session, "block_id": block, "event": {"type": "paste", "payload": {"text": text}}}),
        );
    }
}

fn block_session(store: &Store, block: &str) -> Option<String> {
    store.views.iter().find_map(|(id, view)| {
        let has = view.windows.iter().flat_map(|w| &w.layers).flat_map(|l| &l.blocks).any(|b| b.block_id.as_deref() == Some(block));
        has.then(|| id.clone())
    })
}

/// The machine a block runs on, from the session's layout. Empty if unknown.
fn block_host_id(store: &Store, session: &str, block: &str) -> String {
    store
        .views
        .get(session)
        .into_iter()
        .flat_map(|v| &v.windows)
        .flat_map(|w| &w.layers)
        .flat_map(|l| &l.blocks)
        .find(|b| b.block_id.as_deref() == Some(block))
        .map(|b| b.host_id.clone())
        .unwrap_or_default()
}

/// Shows a transfer's progress in the status bar until it ends, then hands
/// its result to `done`.
fn watch<T: 'static>(
    store: gpui::Entity<Store>,
    job: Task<Result<T>>,
    progress: Arc<Progress>,
    doing: String,
    cx: &mut Context<Workspace>,
    done: impl FnOnce(&gpui::Entity<Store>, Result<T>, &mut App) + 'static,
) {
    cx.spawn(async move |_, cx| {
        let mut job = std::pin::pin!(job);
        let result = loop {
            let tick = cx.background_executor().timer(Duration::from_millis(250));
            match select(job.as_mut(), std::pin::pin!(tick)).await {
                Either::Left((result, _)) => break result,
                Either::Right(_) => {
                    if let Some(shown) = progress.describe() {
                        let text = format!("{doing}… {shown}");
                        if cx.update(|cx| show_notice(&store, text, cx)).is_err() {
                            return;
                        }
                    }
                }
            }
        };
        cx.update(|cx| done(&store, result, cx)).ok();
    })
    .detach();
}

/// How far a transfer has got, shared between the task that moves the bytes
/// and the status bar.
#[derive(Debug, Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

impl Progress {
    fn add(&self, n: usize) {
        self.done.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Such as "42% of 3.1 MB", once the size is known.
    fn describe(&self) -> Option<String> {
        let (done, total) = (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        (total > 0).then(|| format!("{}% of {}", (done.min(total) * 100) / total, human_size(total)))
    }
}

/// A file's size for people: "512 B", "3.1 MB".
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

/// A downloaded file and where it was saved.
#[derive(Debug)]
pub struct Downloaded {
    pub name: String,
    pub saved: PathBuf,
    pub bytes: u64,
}

/// Downloads `path` from the terminal `block`'s machine into `dir`, under a
/// name no file there has yet. A relative path is relative to the working
/// directory of the terminal's foreground process.
pub async fn download(
    control: &Control,
    endpoint: &Endpoint,
    session: &str,
    block: &str,
    path: &str,
    dir: &Path,
    progress: &Progress,
) -> Result<Downloaded> {
    let args = json!({"ref": new_ref("download"), "path": path});
    let begin = control
        .block_call("com.superlogical.terminal.download_begin", session, block, args)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let ticket = begin["ticket"].as_str().context("the server sent no ticket for the download")?;
    let name = safe_name(begin["name"].as_str().unwrap_or(""));
    progress.total.store(begin["size"].as_u64().unwrap_or(0), Ordering::Relaxed);

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (mut file, saved) = create_unique(dir, &name)?;
    let result = async {
        let mut stream = ByteStream::open(endpoint, ticket).await?;
        let mut bytes = 0u64;
        while let Some(chunk) = stream.read().await? {
            file.write_all(&chunk).with_context(|| format!("writing {}", saved.display()))?;
            bytes += chunk.len() as u64;
            progress.add(chunk.len());
        }
        stream.finish().await?;
        anyhow::Ok(bytes)
    }
    .await;
    drop(file);
    match result {
        Ok(bytes) => Ok(Downloaded { name, saved, bytes }),
        Err(e) => {
            // Leave no partial file behind.
            std::fs::remove_file(&saved).ok();
            Err(e)
        }
    }
}

/// A local file to upload.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalFile {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

/// Checks that every path is a regular file, and reads its name and size.
pub fn local_files(paths: &[PathBuf]) -> Result<Vec<LocalFile>> {
    if paths.is_empty() {
        bail!("no files to upload");
    }
    paths
        .iter()
        .map(|path| {
            let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
            if !meta.is_file() {
                bail!("{} is not a file, and only files can be uploaded", path.display());
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(LocalFile { path: path.clone(), name: safe_name(&name), size: meta.len() })
        })
        .collect()
}

/// Uploads files to the terminal `block`'s machine as one batch, so that
/// the terminal types all their paths once the last one arrives.
pub async fn upload(
    control: &Control,
    endpoint: &Endpoint,
    session: &str,
    block: &str,
    files: &[LocalFile],
    progress: &Progress,
) -> Result<()> {
    progress.total.store(files.iter().map(|f| f.size).sum(), Ordering::Relaxed);
    let batch = random_uuid();
    // Reserve every file before sending any, so that a refusal, such as a
    // batch over the server's limits, comes before any bytes are sent.
    let mut tickets = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let mut args = json!({"ref": new_ref("upload"), "name": file.name, "size": file.size});
        if files.len() > 1 {
            args["batch"] = json!(batch);
            args["index"] = json!(index);
            args["count"] = json!(files.len());
        }
        let begin = control
            .block_call("com.superlogical.terminal.upload_begin", session, block, args)
            .await
            .map_err(|e| anyhow::anyhow!("{}: {}", file.name, e.message))?;
        tickets.push(begin["ticket"].as_str().context("the server sent no ticket for the upload")?.to_string());
    }
    // The server ends a batch's streams only once all of its files have
    // arrived, so every stream stays open, and they are sent side by side.
    let sends = files.iter().zip(&tickets).map(|(file, ticket)| async move {
        let mut stream = ByteStream::open(endpoint, ticket).await?;
        send_file(&mut stream, file, progress).await?;
        stream.close_write().await?;
        stream.finish().await.with_context(|| format!("uploading {}", file.name))
    });
    futures::future::try_join_all(sends).await?;
    Ok(())
}

/// Sends exactly `file.size` bytes, the size the server reserved.
async fn send_file(stream: &mut ByteStream, file: &LocalFile, progress: &Progress) -> Result<()> {
    let mut src = std::fs::File::open(&file.path).with_context(|| format!("opening {}", file.path.display()))?;
    let mut buf = vec![0u8; MAX_CHUNK];
    let mut left = file.size;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = src.read(&mut buf[..want]).with_context(|| format!("reading {}", file.path.display()))?;
        if n == 0 {
            bail!("{} got shorter while it was being uploaded", file.path.display());
        }
        stream.write(&buf[..n]).await?;
        left -= n as u64;
        progress.add(n);
    }
    Ok(())
}

/// Whether a block sees the files this machine does, so that local paths
/// can be typed into it as they are. Rex says a block whose `host_id`
/// equals the local server's sees the same files. The server reached over a
/// Unix socket is the local one; a server reached over TCP may be anywhere,
/// so its blocks always get uploads.
pub fn sees_local_files(endpoint: &Endpoint, server_host_id: Option<&str>, block_host_id: &str) -> bool {
    matches!(endpoint, Endpoint::Unix(_)) && !block_host_id.is_empty() && server_host_id == Some(block_host_id)
}

/// Local files' paths as the terminal types them after an upload: each
/// with a backslash before characters the shell treats specially, and a
/// space after each.
pub fn typed_paths(files: &[LocalFile]) -> String {
    files.iter().map(|f| escape_path(&f.path.to_string_lossy()) + " ").collect()
}

/// A path escaped for a POSIX shell with backslashes, as Rex types the
/// paths of uploaded files. A backslash can't escape a control character
/// such as a newline, so a path with one is quoted instead.
fn escape_path(path: &str) -> String {
    if path.chars().any(char::is_control) {
        return shell_quote(path);
    }
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if !(c.is_alphanumeric() || "/._-+:@%,=".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A file name that is safe to create in a directory: no separators or
/// control characters, and never empty, `.` or `..`.
pub fn safe_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').collect();
    let name = name.trim().to_string();
    if name.is_empty() || name == "." || name == ".." { "download".to_string() } else { name }
}

/// The names to try for a download, in order: `name`, then `name (1)`,
/// `name (2)`, and so on, with the number before the extension, as browsers
/// do. A `.tar.gz` style extension stays whole.
pub fn candidate_name(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_string();
    }
    let (stem, ext) = split_extension(name);
    format!("{stem} ({n}){ext}")
}

fn split_extension(name: &str) -> (&str, &str) {
    // A leading dot starts a hidden file's name, not an extension.
    let Some(dot) = name.rfind('.').filter(|&i| i > 0) else { return (name, "") };
    let (stem, ext) = name.split_at(dot);
    if let Some(tar) = stem.strip_suffix(".tar").filter(|s| !s.is_empty()) {
        return (tar, &name[tar.len()..]);
    }
    (stem, ext)
}

/// Creates a new file in `dir` under the first free candidate name. The
/// file is created exclusively, so an existing file is never overwritten,
/// even by a race.
pub fn create_unique(dir: &Path, name: &str) -> Result<(std::fs::File, PathBuf)> {
    for n in 0..10_000 {
        let path = dir.join(candidate_name(name, n));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    bail!("{} has too many files named like {name}", dir.display())
}

/// Where downloads go: ~/Downloads.
fn downloads_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join("Downloads"))
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// A path for the status bar, with the home directory as `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The last element of a path the user typed, for messages.
fn last_component(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_names_before_the_extension() {
        assert_eq!(candidate_name("report.pdf", 0), "report.pdf");
        assert_eq!(candidate_name("report.pdf", 1), "report (1).pdf");
        assert_eq!(candidate_name("archive.tar.gz", 2), "archive (2).tar.gz");
        assert_eq!(candidate_name("Makefile", 1), "Makefile (1)");
        assert_eq!(candidate_name(".bashrc", 1), ".bashrc (1)");
        assert_eq!(candidate_name("a.b.c", 1), "a.b (1).c");
        assert_eq!(candidate_name(".tar.gz", 1), ".tar (1).gz");
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("openrex-unique-{}", random_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "old").unwrap();
        std::fs::write(dir.join("a (1).txt"), "old").unwrap();
        let (_, path) = create_unique(&dir, "a.txt").unwrap();
        assert_eq!(path, dir.join("a (2).txt"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cleans_names() {
        assert_eq!(safe_name("ok.txt"), "ok.txt");
        assert_eq!(safe_name("../x"), "..x");
        assert_eq!(safe_name("a\nb"), "ab");
        assert_eq!(safe_name(".."), "download");
        assert_eq!(safe_name(""), "download");
    }

    #[test]
    fn types_local_paths_only_for_a_local_server_and_the_same_host() {
        let unix = Endpoint::Unix("/run/user/1000/rex/server.sock".into());
        let tcp = Endpoint::Tcp { host: "box".into(), port: 7297 };
        assert!(sees_local_files(&unix, Some("h1"), "h1"));
        assert!(!sees_local_files(&unix, Some("h1"), "h2"));
        assert!(!sees_local_files(&unix, None, "h1"));
        assert!(!sees_local_files(&unix, Some(""), ""));
        assert!(!sees_local_files(&tcp, Some("h1"), "h1"));
    }

    #[test]
    fn quotes_typed_paths() {
        let f = |p: &str| LocalFile { path: p.into(), name: String::new(), size: 0 };
        assert_eq!(typed_paths(&[f("/tmp/a.txt"), f("/tmp/it's here")]), r"/tmp/a.txt /tmp/it\'s\ here ");
        assert_eq!(typed_paths(&[f("/tmp/a\nb")]), "'/tmp/a\nb' ");
        assert_eq!(typed_paths(&[f("/tmp/café (1).txt")]), r"/tmp/café\ \(1\).txt ");
    }

    #[test]
    fn describes_progress_and_sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(3_100_000), "3.1 MB");
        let p = Progress::default();
        assert_eq!(p.describe(), None);
        p.total.store(200, Ordering::Relaxed);
        p.add(50);
        assert_eq!(p.describe().as_deref(), Some("25% of 200 B"));
    }

    #[test]
    fn refuses_directories() {
        let err = local_files(&[std::env::temp_dir()]).unwrap_err();
        assert!(format!("{err}").contains("only files"));
    }

    /// Uploads two files to a terminal of a session of its own, finds the
    /// paths the terminal typed, downloads them back, and compares. Run it
    /// with `cargo test -- --ignored` while a Rex server runs.
    #[test]
    #[ignore = "needs a running Rex server; makes and destroys a session of its own"]
    fn round_trip_through_the_server() {
        smol::block_on(async {
            let endpoint = Endpoint::from_env().unwrap();
            let hello = openrex_proto::Hello { name: "openrex-transfer-test".into(), version: "0".into(), ..Default::default() };
            let conn = openrex_proto::control::connect(&endpoint, hello).await.unwrap();
            let control = conn.control.clone();
            smol::spawn(conn.driver).detach();

            let work = std::env::temp_dir().join(format!("openrex-transfer-{}", random_uuid()));
            std::fs::create_dir_all(work.join("down")).unwrap();
            let layout = json!({"block": {"flavor": crate::store::SHELL_FLAVOR, "options": {"cwd": work.to_string_lossy()}}});
            let made = control
                .call("session.create", json!({"label": format!("openrex-transfer-test-{}", &random_uuid()[..8]), "initial_windows": [{"layout": layout}]}))
                .await
                .unwrap();
            let session = made["session_id"].as_str().unwrap().to_string();
            // Clean up even when an assertion fails.
            let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(round_trip(
                &control, &endpoint, &session, &made, &work,
            )))
            .await;
            control.call("session.destroy", json!({"session_id": session})).await.ok();
            std::fs::remove_dir_all(&work).ok();
            match result {
                Ok(r) => r.unwrap(),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        });
    }

    async fn round_trip(control: &Control, endpoint: &Endpoint, session: &str, made: &Value, work: &Path) -> Result<()> {
        let block = made["initial_windows"][0]["block_ids"][0].as_str().context("no block")?;
        control.call("session.attach", json!({"session_id": session})).await?;
        // Two files, one over a message long, one with a name to quote.
        let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let name = format!("it's {}.bin", &random_uuid()[..8]);
        std::fs::write(work.join("big.dat"), &big)?;
        std::fs::write(work.join(&name), b"small")?;
        let files = local_files(&[work.join("big.dat"), work.join(&name)])?;
        let progress = Progress::default();
        upload(control, endpoint, session, block, &files, &progress).await?;
        assert_eq!(progress.done.load(Ordering::Relaxed), 200_005);

        // The prompt shows the working directory too, so pick out the
        // uploaded files' paths by their names.
        let suffixes = ["/big.dat".to_string(), format!("/{name}")];
        let words = typed_words(control, session, block, |w| suffixes.iter().any(|s| w.ends_with(s.as_str())), 2).await?;
        assert!(words[0].ends_with("/big.dat") && words[1].ends_with(&suffixes[1]), "{words:?}");

        let down = work.join("down");
        let got = download(control, endpoint, session, block, &words[0], &down, &Progress::default()).await?;
        assert_eq!((got.saved.clone(), got.bytes), (down.join("big.dat"), 200_000));
        assert!(std::fs::read(&got.saved)? == big);
        let again = download(control, endpoint, session, block, &words[1], &down, &Progress::default()).await?;
        assert_eq!(std::fs::read(&again.saved)?, b"small");
        // The same file again gets a new name, and a relative path works.
        let third = download(control, endpoint, session, block, "big.dat", &down, &Progress::default()).await?;
        assert_eq!(third.saved, down.join("big (1).dat"));
        assert!(download(control, endpoint, session, block, "missing.txt", &down, &Progress::default()).await.is_err());
        assert_eq!(std::fs::read_dir(&down)?.count(), 3);

        // The terminal runs on this machine, so a drop would type the local
        // paths, and they read back as the same words.
        if matches!(endpoint, Endpoint::Unix(_)) {
            let status = control.call("server.status", json!({})).await?;
            let view: openrex_proto::types::SessionView =
                serde_json::from_value(control.call("session.view", json!({"session_id": session})).await?)?;
            let host = &view.windows[0].layers[0].blocks[0].host_id;
            assert!(sees_local_files(endpoint, status["host_id"].as_str(), host));
            let text = typed_paths(&files);
            let event = json!({"type": "paste", "payload": {"text": text}});
            control.call("block.event", json!({"session_id": session, "block_id": block, "event": event})).await?;
            // They read back as the local paths, word for word.
            let want: Vec<String> = files.iter().map(|f| f.path.to_string_lossy().into_owned()).collect();
            let words = typed_words(control, session, block, |w| want.iter().any(|p| p.as_str() == w), 2).await?;
            assert_eq!(words, want);
        }
        Ok(())
    }

    /// Waits up to five seconds for a line of the terminal's screen with
    /// `count` words that `keep` picks, and returns them in order.
    async fn typed_words(
        control: &Control,
        session: &str,
        block: &str,
        keep: impl Fn(&str) -> bool,
        count: usize,
    ) -> Result<Vec<String>> {
        for _ in 0..100 {
            let screen = control
                .block_call("com.superlogical.terminal.format", session, block, json!({"format": "text", "unwrap": true}))
                .await?;
            let content = screen["content"].as_str().unwrap_or("").to_string();
            for line in content.lines().rev() {
                let words: Vec<String> = shell_words(line).into_iter().filter(|w| keep(w.as_str())).collect();
                if words.len() == count {
                    return Ok(words);
                }
            }
            smol::Timer::after(Duration::from_millis(50)).await;
        }
        bail!("the terminal never showed the typed paths")
    }

    /// Splits text the way a POSIX shell splits words, for single quotes
    /// and backslashes, which is enough for typed paths.
    fn shell_words(s: &str) -> Vec<String> {
        let (mut words, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '\'' => {
                    quoted = !quoted;
                    any = true;
                }
                '\\' if !quoted => cur.extend(chars.next()),
                c if c.is_whitespace() && !quoted => {
                    if any || !cur.is_empty() {
                        words.push(std::mem::take(&mut cur));
                    }
                    any = false;
                }
                c => cur.push(c),
            }
        }
        if any || !cur.is_empty() {
            words.push(cur);
        }
        words
    }

    #[test]
    fn names_the_last_component() {
        assert_eq!(last_component("~/logs/app.log"), "app.log");
        assert_eq!(last_component("dir/"), "dir");
        assert_eq!(last_component("x"), "x");
    }
}
