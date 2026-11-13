//! OpenRex: a Linux client for the Rex terminal multiplexer server.

use anyhow::{Context as _, Result, bail};
use gpui::{App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use openrex_proto::Endpoint;

use openrex::store::{Settings, Store};
use openrex::workspace::Workspace;

const USAGE: &str = "\
Usage: openrex [OPTIONS] [ENDPOINT]

A Linux client for the Rex terminal multiplexer server.

ENDPOINT is where the server listens: a Unix socket path or unix:// URL,
host:port for a server reached over TCP, or https://host for one behind TLS,
such as built-in Tailscale. It defaults to REX_SERVER, then to
the local server's socket, $XDG_RUNTIME_DIR/rex/server.sock.

Options:
  --font FAMILY     Terminal font (default: the system's monospace font)
  --font-size PX    Terminal font size in pixels (default: 14)
  --theme NAME      Colour theme: a built-in one or a Ghostty theme's name
  --label LABEL     Name this client, so `rex -C LABEL` can address it
  -h, --help        Show this help
  -V, --version     Show the version

Settings saved in ~/.config/openrex/settings.json (the theme, font_family, and
font_size) apply unless an option overrides them.
";

struct Args {
    endpoint: Endpoint,
    settings: Settings,
    theme: Option<openrex::theme::TermTheme>,
    settings_file: Option<openrex::theme::SettingsFile>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut endpoint = None;
    let mut font = None;
    let mut font_size = None;
    let mut theme = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("openrex {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--font" => font = Some(args.next().context("--font needs a font family")?),
            "--theme" => theme = Some(args.next().context("--theme needs a theme's name")?),
            "--label" => {
                let label = args.next().unwrap_or_default();
                let label = openrex::server::check_label(&label).map_err(anyhow::Error::msg)?;
                openrex::server::set_client_label(label);
            }
            "--font-size" => {
                let value = args.next().context("--font-size needs a size in pixels")?;
                font_size = Some(value.parse().ok().filter(|s| (6.0..=72.0).contains(s)).with_context(|| {
                    format!("--font-size takes a number from 6 to 72, not {value}")
                })?);
            }
            s if s.starts_with('-') => bail!("unknown option {s}\n\n{USAGE}"),
            s if endpoint.is_none() => endpoint = Some(Endpoint::parse(s)?),
            s => bail!("unexpected argument {s}\n\n{USAGE}"),
        }
    }
    let endpoint = match endpoint {
        Some(e) => e,
        None => Endpoint::from_env()?,
    };
    // The options win over the settings file.
    let settings_file = openrex::theme::SettingsFile::standard();
    let saved = match settings_file.as_ref().map(|f| f.load()) {
        Some(Ok(saved)) => saved,
        Some(Err(e)) => {
            eprintln!("openrex: {e}");
            Default::default()
        }
        None => Default::default(),
    };
    let theme = match theme {
        Some(name) => Some(openrex::theme::find(&name).with_context(|| format!("there is no theme named {name}"))?),
        None => saved.theme.and_then(|name| {
            let found = openrex::theme::find(&name);
            if found.is_none() {
                eprintln!("openrex: the saved theme {name} isn't installed; using {}", openrex::theme::DEFAULT_THEME);
            }
            found
        }),
    };
    let font_family = font.or(saved.font_family).unwrap_or_else(system_monospace);
    let font_size = font_size.or(saved.font_size).unwrap_or(14.0);
    Ok(Some(Args { endpoint, settings: Settings { font_family, font_size }, theme, settings_file }))
}

/// The family fontconfig picks for `monospace`.
fn system_monospace() -> String {
    std::process::Command::new("fc-match")
        .args(["-f", "%{family}", "monospace"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split(',').next().map(|f| f.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "monospace".to_string())
}

fn main() {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return,
        Err(e) => {
            eprintln!("openrex: {e:#}");
            std::process::exit(2);
        }
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    Application::new().run(move |cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let Args { endpoint, settings, theme, settings_file } = args;
        openrex::theme::init(settings_file, theme);
        let store = cx.new(|cx| Store::new(endpoint, settings, cx));
        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("OpenRex".into()), ..Default::default() }),
                app_id: Some("openrex".to_string()),
                window_min_size: Some(size(px(480.0), px(300.0))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(store, window, cx)),
        );
        if let Err(e) = opened {
            eprintln!("openrex: could not open a window: {e:#}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}
