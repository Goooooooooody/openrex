# OpenRex

A Linux client for the [Rex](https://www.superlogical.com/rex/docs) terminal
multiplexer server, written in Rust with [gpui](https://gpui.rs).

Rex keeps your terminals running on a server. Its own app is for macOS.
OpenRex connects to the same server over the API the server documents about
itself, so sessions you start in one client show up in the other.

OpenRex is not made by or affiliated with Superlogical, the makers of Rex.

## What works

- Sessions in a sidebar: switch, create, and end them.
- Windows as tabs: create, close, and switch.
- Panes: split right or down, close, zoom, move focus, and drag the border
  between two panes to resize them.
- Terminals drawn with Ghostty's terminal engine, the one the server uses, so
  the screen, scrollback, colors, wide characters, and full-screen programs
  match the server exactly.
- Keyboard input, paste, and mouse and wheel reporting for programs that ask
  for them.
- Local scrollback, selection by dragging, double click (word) and triple
  click (line), copy, and middle-click paste.
- Titles, desktop notifications, and clipboard writes from programs.

Not yet: input methods and dead keys, images (Kitty graphics), clickable
links, search, the server's Lua key bindings, themes, file upload and
download, floating-layer management, and a host list for remote servers.

## Requirements

- A Rex server. See the [Linux install page](https://www.superlogical.com/rex/docs/linux/installation).
- Rust 1.90 or later.
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

| Key | Action |
| --- | --- |
| Ctrl+Shift+N | New session |
| Ctrl+Shift+T | New tab |
| Ctrl+Shift+D | Split right |
| Ctrl+Shift+E | Split down |
| Ctrl+Shift+W | Close pane |
| Ctrl+Shift+Enter | Zoom pane |
| Ctrl+Alt+Arrow | Focus the pane in that direction |
| Ctrl+Tab, Ctrl+Shift+Tab | Next tab, previous tab |
| Ctrl+Shift+C, Ctrl+Shift+V | Copy, paste |
| Shift+PageUp, Shift+PageDown | Scroll back, scroll forward |
| Ctrl+Shift+R | Reconnect |
| Ctrl+Shift+Q | Quit |

Every other key goes to the terminal. Hold Shift to select with the mouse
while a program is using it.

## How it fits together

- `crates/openrex-proto` speaks the server's protocol and knows nothing about
  the UI: the control connection (JSON over a WebSocket, for method calls
  and events), data connections (one byte stream per terminal), and the
  framing of a terminal stream.
- `crates/openrex` is the app. `store` holds the sessions and their layout
  and reacts to server events. `term` keeps a local copy of each terminal by
  feeding the server's snapshot and output to Ghostty's engine. `pane` draws
  a terminal and sends its input. `workspace` is the window.

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
