# Notice
Community contributions are welcome and encouraged, this is intended as a canvas, not a finished product.

If you fork it or adopt it into your own thing, please credit the original and push fixes upstream :) 

# OpenRex

A Linux client for the [Rex](https://www.superlogical.com/rex/docs) terminal
multiplexer server, written in Rust with [gpui](https://gpui.rs).

Rex keeps your terminals running on a server. Its own app is for macOS.
OpenRex connects to the same server over the API the server documents about
itself, so sessions you start in one client show up in the other.

OpenRex is not made by or affiliated with Superlogical, the makers of Rex.

## What works

- **Sessions, tabs, and panes.** Sessions in a sidebar, windows as tabs, and
  panes you can split in any direction, resize by dragging or by key, swap,
  balance, zoom, detach and bring back, and move to another tab. New panes
  start in the directory the focused terminal last reported. Double-click a
  tab or session to rename it; middle-click a tab to close it.
- **Floating panes.** Float a pane or open a new floating one, then move it by
  its title bar, resize it from its corner, minimize it to a chip in the tab
  bar, dock it back into the layout, or close it.
- **Terminals drawn with Ghostty's engine**, the one the server uses, so the
  screen, scrollback, colors, wide characters, and full-screen programs match
  the server exactly. Every underline style (single, double, curly, dotted,
  dashed), underline colors, strikethrough, and overline. **Kitty graphics**
  images.
- **Find** in the screen and scrollback, with every match highlighted.
- **Copy or save the scrollback** as text or as HTML with its colors
  (`format`), and **Pane Info**: the foreground process, directory, size and
  owner, changed modes, and input mode. The status bar says when a program is
  reading input without echo, as at a password prompt. Copies of any size,
  from programs or from another client's selection, reach the clipboard.
- **Everything else the API offers a person**: run another client's actions
  (such as Rex.app's), call any method of a pane's block and read the result,
  open a tab with any kind of block the server makes, start detached panes,
  send a floating pane to the back, list the session's clients, set this
  client's label while it runs, and sign the server out of Tailscale or
  rename it there. Session names resolve through the server, so any label or
  ID ending works. On quit, OpenRex gives up the panes it sized so another
  client can size them. **Go to Target…** completes sessions, tabs, and panes
  by label, tab number, or ID as you type (`target.complete`); the **Server**
  and **Session API Reference** list every method; **Stop Server…** asks
  before it stops anything; **Stop Following Other Sessions** detaches from
  background sessions; and **System Permissions** shows what macOS lets a Mac
  server use. When files dropped on a pane are on the server's own machine,
  OpenRex checks the server can read them before typing their paths, and
  uploads them otherwise.
- **Blocks that aren't terminals**, such as Rex's onboarding walkthrough,
  show as a card you can restart or close.
- **Links**: web links and OSC 8 hyperlinks open with Ctrl+click.
- **Activity**: what programs report (OSC 7501 program status, OSC 9;4
  progress), bells, and exits show on tabs and sessions, with an activity
  list in the sidebar and in **View Activity…**. Background tabs show their
  terminals' real titles.
- **Keys and actions the way Rex does them.** OpenRex declares its actions
  under Rex.app's names and follows the key bindings in your Rex
  configuration file (`init.lua`): bindings to actions, key sequences such as
  `ctrl+b>c`, key modes, and Lua functions. Configuration-file actions and
  block actions run from keys and the palette.
- **A command palette** (Ctrl+Shift+P) with every action and its keys. It is
  also how you switch sessions, go to any session, tab, or pane, rename
  things, and type paths, with completion from the terminal's machine.
- **OpenRex's own Lua scripts.** Files in `~/.config/openrex/scripts/*.lua`
  load into the server for this client only (`client.set_script`), with the
  same API as `init.lua`; **Reload OpenRex Scripts** loads them again, and
  their problems show in **Rex Diagnostics**.
- **Scriptable from outside.** Other clients and scripts can ask OpenRex to
  perform its actions: `rex -C <client> do pane.split direction=down`.
- **Files**: drop files on a pane, or use **Upload Files…**, to send them to
  the terminal's machine and type their paths; **Download File…** saves a file
  from it to `~/Downloads`.
- **Themes**: ten built-in themes plus every Ghostty theme file installed,
  applied to the terminals and to the window around them. The theme and font
  are saved in `~/.config/openrex/settings.json`.
- **Input methods, dead keys, and compose**, with the composition drawn at
  the cursor.
- **Shell prompts**: jump to the previous or next prompt in the scrollback
  (needs shell integration that marks prompts, OSC 133).
- **The server**: **Rex Diagnostics** shows the server's version, status,
  identity, configuration problems, and connected clients. Turn built-in
  Tailscale on and off, and sign it in. Add, remove, and switch between the
  server's hosts, including servers reached over HTTPS.
- Keyboard input, paste, mouse and wheel reporting, local scrollback,
  selection (drag, double click for a word, triple click for a line), copy,
  middle-click paste, titles, desktop notifications, and clipboard writes from
  programs, even while the window isn't focused.

## Requirements

- A Rex server. See the [Linux install page](https://www.superlogical.com/rex/docs/linux/installation).
- A recent stable Rust.
- Zig 0.17, which builds the terminal engine. `mise.toml` pins it.
- Network access on the first build, which downloads Ghostty's source.

## Build and run

```sh
mise install            # Zig 0.17
mise exec -- cargo build --release
./target/release/openrex
```

With no argument, OpenRex connects to the local server's socket,
`$XDG_RUNTIME_DIR/rex/server.sock`, or to the server `REX_SERVER` names.
Pass a socket path or `host:port` to connect elsewhere:

```sh
openrex /run/rex/server.sock
openrex devbox:7297
```

`openrex --help` lists the options: `--font` and `--font-size`.

## Keys

These are the defaults. Your Rex configuration file can change any of them
with `rex.bind`, using the action names that **Command Palette…** and
`rex -C <client> actions` list.

| Key | Action |
| --- | --- |
| Ctrl+Shift+P | Command palette |
| Ctrl+Shift+N, Ctrl+Shift+K | New session, change session |
| Ctrl+Shift+O | Go to anything |
| Ctrl+Shift+T | New tab |
| Ctrl+Tab, Ctrl+Shift+Tab | Next tab, previous tab |
| Alt+1 … Alt+9 | Go to tab |
| Ctrl+Shift+PageUp, Ctrl+Shift+PageDown | Move tab |
| Ctrl+Shift+D, Ctrl+Shift+E | Split right, split down |
| Ctrl+Shift+W | Close pane |
| Ctrl+Shift+Enter | Zoom pane |
| Ctrl+Alt+Arrow | Focus the pane in that direction |
| Ctrl+Alt+Shift+Arrow | Swap with the pane in that direction |
| Ctrl+Alt+Enter | Float or dock the pane |
| Ctrl+Alt+M, Ctrl+Alt+Shift+Enter | Minimize floating pane, all floating panes |
| Ctrl+Shift+C, Ctrl+Shift+V, Ctrl+Shift+A | Copy, paste, select all |
| Ctrl+Shift+F | Find |
| Shift+PageUp, Shift+PageDown | Scroll back, scroll forward |
| Ctrl+Shift+Up, Ctrl+Shift+Down | Previous prompt, next prompt |
| Ctrl+Shift+G | Go to directory (new tab) |
| Ctrl+=, Ctrl+-, Ctrl+0 | Font size |
| Ctrl+Shift+B, Ctrl+Shift+. | Toggle sidebar, toggle activity |
| Ctrl+Shift+R | Reconnect |
| Ctrl+Shift+Q | Quit (the server keeps everything running) |

Every other key goes to the terminal. Hold Shift to select with the mouse
while a program is using it; hold Ctrl to open links.

## How it fits together

- `crates/openrex-proto` speaks the server's protocol and knows nothing about
  the UI: the control connection (JSON over a WebSocket, for method calls
  and events), data connections (one byte stream per terminal), and the
  framing of a terminal stream.
- `crates/openrex` is the app. `store` holds the sessions and their layout
  and reacts to server events. `term` keeps a local copy of each terminal by
  feeding the server's snapshot and output to Ghostty's engine. `pane` draws
  a terminal and sends its input. `workspace` is the window.
- `keymap` reads keys in Rex's spelling and resolves bindings, sequences, and
  modes; `actions` performs every action, whether a key, the palette
  (`palette`), or another client asked for it. Each larger feature has its
  own module: `find`, `links`, `activity`, `layers`, `transfer`, `theme`,
  `server`, `input`, and `graphics`.

The server describes its API at `GET /llms.txt` on its socket:

```sh
curl -s --unix-socket "$XDG_RUNTIME_DIR/rex/server.sock" http://rex/llms.txt
```

## Tests

```sh
mise exec -- cargo test
```

Tests that need a running server are ignored by default. They create their
own sessions and destroy them:

```sh
mise exec -- cargo test -p openrex --test live_server -- --ignored
```

To drive the window without a keyboard, set `OPENREX_REPLAY` to a list of
key presses, such as `ctrl-shift-d wait@1000 l s enter`. See
`crates/openrex/src/replay.rs`.

## Contributing

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md) for how to
build, test, and send a pull request, and the
[Code of Conduct](CODE_OF_CONDUCT.md). Report security problems privately, as
[SECURITY.md](SECURITY.md) describes.

## License

OpenRex is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in OpenRex by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
