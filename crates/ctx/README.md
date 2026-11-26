# ctx

ctx keeps the pieces of each piece of work together as a project and brings back your place when
you return to it. A project groups ADO PRs and work items, GitLab MRs, git branches, files, URLs and
research notes. Freezing a project writes a short handoff and saves a snapshot of its items, so
coming back shows what changed since you left. Wait conditions on live or frozen projects clear
themselves, so projects that are ready to work on come to the top.

ctx is becoming a desktop app with its own terminals. The tmux interface is gone. On macOS,
`ctx` with no arguments opens the app, which so far is one window with a terminal running your
login shell. Until the app shows projects, ctx 0.3.0 stays the version for daily use. Both
versions read and write the same project files.

## Setup

Build and install both binaries from the repository root:

```sh
just install
```

ctx uses the same `az`, `glab` and `claude` logins as rvt. The first run writes
`~/.config/ctx/config.toml` and stops until you fill in `[ado]`, `[gitlab]`, or both. These take
the same fields as rvt's (see [the rvt README](../../README.md#configuration)). ctx also reads
`ado.work_item_types` and `ado.done_states`, which choose the work items listed as yours. Their
defaults, `Bug` and `Product Backlog Item` until `Done`, `Removed` or `Closed`, match the Scrum
process. Without `[ado]`, ctx skips ADO items, and without `[gitlab]`, it skips GitLab items.
The `[workspace]` section was called `[tmux]` before, and ctx still reads it under that name.

To let Claude Code sessions clear waits, merge the output of `ctx hooks` into the
`settings.json` of each Claude config directory you use, such as `~/.claude/settings.json`. The
hooks do nothing outside a terminal of the ctx app.

To have Claude record its finished work in project journals, run `ctx skill --write`. It installs
the `ctx-update` skill under the Claude config directory set by `config_dir` in the `[claude]`
section, or under `~/.claude` when that is empty.

## Commands

`ctx --help` lists every command. Most take `--project <name>`. In a terminal of the ctx app they
default to that terminal's project, which the app names in `CTX_PROJECT`.

- `ctx list` shows the projects and their state, `ctx done` hides a finished project and
  `ctx reopen` makes it live again.
- `ctx wait <condition>` sets what a project waits on, and `ctx wait --clear` removes it.
- `ctx note`, `ctx clip`, `ctx import` and `ctx link` add notes, files and items.
- `ctx search <words>` searches notes, handoffs, the journal and item text in the current
  project, or in all projects with `--all`.
- `ctx refresh` fetches the items of every project, or of one, and clears the waits that are met.
- `ctx run -- <command>` runs a command and makes the project wait until it exits.
- `ctx summarize <project>` and `ctx brief <project>` ask Claude for a summary of the last
  handoff or a brief on picking the project back up.

### Wait conditions

A wait condition says what a project waits on. It can be:

- Claude finishing or asking for input, in any terminal of the project or only in this one
- a `ctx run` command exiting
- a review on a PR or MR: an approval, a vote for changes, or a comment, whichever comes first
- a PR or MR merged or closed, or its CI finishing
- a new comment on a work item
- a work item reaching a state
- a date passing
- free text that ctx shows but cannot check

`ctx wait claude` in a terminal of the ctx app waits on the Claude session in that terminal, and
`--any` waits on any Claude session in the project.

### Notes

- `ctx clip` saves the clipboard, or stdin, as a note and asks for a title and source.
- `ctx note` opens `$EDITOR` on a new note.
- `ctx import <link or words>` asks Claude to find a Teams thread or email through the
  Microsoft 365 connector and saves a summary with the source link. Claude gets only the tools in
  `tools` under `[import]`, which default to the connector's read tools. An empty list turns
  import off.
- `ctx link <path>` adds a local file, such as a plan, which is indexed where it is.

### Claude

The hooks from `ctx hooks` run `ctx event`, which records Claude starting, stopping, asking for
input and ending in the project's `events.jsonl`. The ctx app sets `CTX_PROJECT` and
`CTX_TERMINAL` in each of its terminals, so each event names the project and the terminal it came
from.

When Claude works in a linked git worktree, the hooks add the worktree's branch to the project,
stored against the repository's main checkout. ctx checks at the start of each session and at the
end of each turn, and skips a branch the project already holds and a detached HEAD.

### Updates from Claude

`ctx update` adds a summary of finished work to a project's journal. With the `ctx-update` skill
installed, Claude runs it at the end of work on a PR, MR, work item or branch, and the journal
entry is headed `Claude:` and the items' labels.

```sh
ctx update pr:4711 --message "Addressed the review threads. One waits on the reviewer."
ctx update wi:98123 --add pr:4720 --message "Opened PR 4720 with the fix."
```

ctx writes to every project that is not done and that lists one of the items. A branch in a
worktree counts as the same branch in the main checkout. When no project lists the items, ctx
looks for projects holding an item linked to one of them, such as the PR of a listed branch or
the work item of a listed PR. When none does either, the entry goes to the current project, and
the items are added to it. `--add` adds items Claude created, such as a new PR for a listed work
item, to each project the entry went to. `--message -` reads the summary from stdin, and
`--project` names the project instead of searching.

## Files

```text
~/.config/ctx/config.toml
~/.local/share/ctx/
  projects/<slug>/
    project.toml      name, state, workspace and layout, items, wait condition
    journal.md        handoffs, state changes and Claude summaries, newest last
    snapshot.json     item states at the last freeze
    visit.json        item states when you last left the project
    events.jsonl      Claude and ctx run events, for wait conditions
    brief.json        the last Claude brief
    responses/        Claude's responses to PR and MR threads, one file per item
    notes/<date>-<title>.md
  cache.json          the latest state of every item
  inbox.json          your PRs, MRs and work items, for the app's Unfiled section
  index.db            the search index; ctx reindex rebuilds it
```

The `projects` directory is a git repository, and ctx commits after each freeze and state change.
`CTX_HOME` and `CTX_CONFIG_HOME` move the two directories, for testing.
