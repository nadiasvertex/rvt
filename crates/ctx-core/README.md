# ctx-core

ctx-core holds ctx's projects and the work done on them, without a user interface. It covers the
project store and journal, item references and refresh, the Unfiled inbox, wait conditions and
the watcher, search, Claude summaries, briefs and review responses, and the operations that
change a project. The `ctx` binary in `crates/ctx` is built on it, and
[its README](../ctx/README.md) describes how ctx works for its users.

## Front ends

ctx-core reaches terminals only through the `Terminals` trait in `terminals.rs`. A front end
implements it to open, show and close a project's workspace, save its layout, show messages,
and type into a Claude session's terminal. Operations that need terminals, such as
`ops::switch`, `ops::freeze`, `ops::evaluate` and `watch::run`, take a `&dyn Terminals`. The
`ctx` command line passes `Cli`, which has no workspaces and refuses what only the ctx app can
do. The ctx app will pass its own front end.

Project files keep the names the tmux front end gave these values, so older ctx versions can
still read them: `session` holds `Project::workspace`, `panes` holds `Window::terminals`, and
`pane` holds a terminal ID in Claude session records, events and Claude waits.

ctx-core does not depend on ratatui, GPUI or other UI crates. `just check-core` checks this, and
`just test` runs it.

## Tests

`store::testing::TempHome` points `CTX_HOME` at a fresh directory for one test. Other crates
get it by enabling the `testing` feature in their `[dev-dependencies]`.
