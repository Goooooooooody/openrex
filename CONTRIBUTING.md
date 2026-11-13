# Contributing to OpenRex

OpenRex is a community project: a canvas, not a finished product. Fixes,
features, bug reports, and ideas are all welcome.

## Getting set up

You need a Rex server, a recent stable Rust, and Zig (pinned in `mise.toml`;
`mise install` fetches it). The first build downloads Ghostty's source.

```sh
mise install
mise exec -- cargo build
mise exec -- cargo run --release
```

## Before you open a pull request

CI runs these on every pull request, and they must pass:

```sh
mise exec -- cargo fmt --all --check
mise exec -- cargo clippy --all-targets -- -D warnings
mise exec -- cargo test
```

Some tests need a running Rex server and are skipped by default. Run them when
you change the protocol, the store, or input. They make their own sessions and
destroy them:

```sh
mise exec -- cargo test -- --ignored
```

Changes you can see deserve a screenshot in the pull request.

## How the work flows

- `main` is always releasable. It's protected: changes arrive through pull
  requests, which need CI to pass and a maintainer's approval.
- Work on a branch in your fork, named for what it does (`fix-find-wrap`,
  `feature-pane-headers`), and open a pull request against `main`.
- Pull requests are squash-merged, so the title becomes the commit message on
  `main`: write it as a short sentence about the change.
- Releases are tags on `main`, such as `v0.2.0`. Pushing one builds OpenRex and
  publishes a GitHub release with a Linux binary. A tag with a hyphen, such as
  `v0.3.0-rc.1`, makes a pre-release.

## Where things live

The README's "How it fits together" section maps the code. In short:
`openrex-proto` speaks the server's protocol and knows nothing about the UI;
`openrex` is the app, with one module per larger feature (`find`, `links`,
`activity`, `layers`, `transfer`, `theme`, `server`, `input`, `graphics`,
`inspect`, `extras`). New actions go through `actions.rs` or a feature
module's `perform`, and are declared in `keymap.rs` under Rex.app's names
when Rex has one, so one `init.lua` works with both.

The server documents its own API:

```sh
curl -s --unix-socket "$XDG_RUNTIME_DIR/rex/server.sock" http://rex/llms.txt
```

## Style

- Comments explain why, in plain sentences.
- Never panic on data from the server: read JSON tolerantly, because newer
  servers add fields and values.
- Keep shared files (`workspace.rs`, `store.rs`, `pane.rs`) small edits; put a
  feature's logic in its own module.
- Add unit tests for pure logic: parsing, geometry, matching.

## Licence

OpenRex is dual-licensed under MIT or Apache-2.0, at your option. Unless you
say otherwise, any contribution you submit for inclusion in OpenRex, as
defined in the Apache-2.0 licence, is dual-licensed the same way, without any
additional terms or conditions.

If you fork OpenRex or build on it, please credit the original and send your
fixes upstream.

## Conduct

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
