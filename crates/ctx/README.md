# ctx

ctx keeps the pieces of each piece of work together as a project and brings back your place when
you return to it. A project groups ADO PRs and work items, GitLab MRs, git branches, files, URLs and
research notes. Switching moves between projects and leaves each one live. Freezing is a separate
choice that writes a short handoff, saves a snapshot of the items and can close the tmux session.
Coming back to a project shows what changed since you left. Wait conditions on live or frozen
projects clear themselves, so projects that are ready to work on come to the top.

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

Add the tmux bindings, then reload tmux:

```sh
ctx tmux-config --write
echo 'source-file ~/.config/ctx/tmux.conf' >> ~/.tmux.conf
tmux source-file ~/.tmux.conf
```

This binds `prefix j` to the project picker, shows `ctx status` next to the clock, keeps
`ctx watch` running, and sets `detach-on-destroy off` so that closing a session moves you to
another one.

To let Claude Code sessions clear waits and offer `claude --resume`, merge the output of
`ctx hooks` into the `settings.json` of each Claude config directory you use, such as
`~/.claude/settings.json`. The hooks do nothing outside a ctx session.

To have Claude record its finished work in project journals, run `ctx skill --write`. It installs
the `ctx-update` skill under the Claude config directory set by `config_dir` in the `[claude]`
section, or under `~/.claude` when that is empty.

## Daily use

One terminal is enough. Each project has its own tmux session, and switching moves your tmux
client from one session to another, so the terminal stays the same. Run `ctx` once to get in, and
use `prefix j` from then on.

Start a project with `n` in the picker. The dialog asks for a name and a directory, which starts
as the directory of the pane you opened the picker from. In a git repository, the current branch
becomes the first item, and its PR or MR shows up as a suggestion after the first refresh. From a
shell, `ctx new "markdown serial"` does the same in the current directory.

Window 0 of each session runs ctx on the project screen, and the other windows are yours. Press
`prefix j` anywhere to open the picker:

- `Esc` closes it and leaves you where you were.
- `Enter` switches to the selected project. The project you leave stays live.
- `o` opens a project's screen without switching.
- `f` freezes a project, `t` thaws it, `w` sets a wait condition, `d` marks it done.
- `C` starts Claude in the project's session and switches there. If Claude isn't in the
  session yet, meaning no window named `claude` and no pane running it, it splits the current
  window side by side and starts Claude in the new pane.
  Otherwise it opens a new window.

From a shell, the same actions are `ctx sw`, `ctx freeze`, `ctx thaw`, `ctx wait` and `ctx done`.
`ctx --help` lists every command.

### Unfiled work

The home screen lists your work that is in no project under Unfiled. This covers active ADO PRs
you created, Bugs and Product Backlog Items assigned to you that are not Done, Removed or Closed,
and open GitLab MRs you created. An item leaves the section once any project holds it, including
a done project.

- `a` adds the selected item to a project. Type to filter the list. Projects that already suggest
  the item come first.
- `n` starts a new project named after the item, with the item as its first item. ctx stays on
  the home screen, so you can file several items in a row.
- `⏎` opens the item in the browser.

ctx looks for these items when it starts, on `R`, on `ctx refresh`, and on each `ctx watch`
round. When a query fails, the section heading shows the error and keeps what that query found
last time.

### Reading items and notes

ctx renders Markdown the way rvt does. PR, MR and work item descriptions, notes, linked `.md`
files, the brief and journal entries are styled for the terminal. Comments show as threads, in the
same cards as rvt: a gutter colored by status, one color per author, your own comments marked
`(you)`, and a last line that says whose turn it is. The detail pane sits beside the project
screen when the terminal is at least 110 columns wide, and `v` shows it full screen at any width.

- `J` and `K` scroll the detail pane.
- `v` reads the selected item or note full screen. `j` `k`, space and `b`, and `gg` `G` scroll,
  and `Esc` goes back.
- `u` shows or hides resolved threads, which are hidden by default.

Items cached by an older ctx show their plain text until the next refresh.

### Answering review comments

`r` on a PR or MR asks Claude to work through the open threads that wait on you, the ones counted
by `↩`. Claude reads the code in your local checkout of the PR's branch and, for each thread, gives
an assessment, a plan for the fix, and a draft reply. The assessment is one of agree, partly,
disagree, or answer (a question that needs only a reply). The checkout is any worktree that has
the branch checked out, in the repository of a branch item for that branch or of the project's
directory. When none has it, ctx fetches the branch and adds a worktree for it at
`checkouts/<slug>/<branch>` in ctx's data directory. The worktree stays, so the fixes can be made
there.
Claude only reads. It takes a few minutes, and you can keep using ctx meanwhile. While it works,
the item's row says `Claude working`, and the header and the detail pane show the current step,
such as loading the threads or Claude reading the code, with the time so far. When it is done,
the row shows how many drafted replies are not posted yet, such as `2 drafts`. When it fails, the
row says `Claude failed`, and the detail pane gives the reason until you ask again.

`r` on an item with drafts opens the responses: a checklist at the top, and below it the selected
thread with Claude's assessment, plan, and draft reply. The line between them says what `p` will
post, and the end of the detail says what posting does to the thread.

- `p` posts the reply below, after `y` confirms. When responses are checked, it posts those
  instead. Posted replies show `[✓]` and are not posted again.
- `r` sets whether posting the reply also resolves its thread. Threads stay open unless you
  choose this, and the row shows `resolves` for those that will be resolved. On Azure DevOps a
  thread where Claude disagrees is resolved as "won't fix", and others as "resolved". For a reply
  that is already posted, `r` and then `p` resolves the thread.
- `space` checks a response, and `a` checks or clears them all.
- `e` opens the draft reply in `$EDITOR`.
- `C` saves the checked plans as a note and starts Claude in the project's tmux session to make
  the fixes. The new session is told to check each plan against the code and not to post to the
  PR.
- `R` asks Claude again, for example after new comments. The view says when there are comments
  newer than the responses, or when the checkout was not at the PR's head commit.

Responses are saved under the project's `responses/` directory. `respond_model` in the `[claude]`
section picks the model, `opus` by default.

### PR and MR state

The project screen shows three markers after an open PR's or MR's status, and the detail pane
spells them out:

- `⚠` the PR conflicts with its target branch.
- `✓`, `✗` or `◐` the build passed, failed or is running. On Azure DevOps this is the PR's build
  policies; required builds decide when there are any. On GitLab it is the head pipeline.
- `↩2` two threads wait for your reply. A thread counts when it is open, someone else wrote
  last, and it is on your own PR or you wrote in it.

### Wait conditions

A wait condition says what a project waits on. It can be:

- Claude finishing or asking for input
- a `ctx run` command exiting
- a review on a PR or MR: an approval, a vote for changes, or a comment, whichever comes first
- a PR or MR merged or closed, or its CI finishing
- a new comment on a work item
- a work item reaching a state
- a date passing
- free text that ctx shows but cannot check

To look at something else while Claude works, press `prefix j`, then `w`, and choose "Claude
finishes in this pane". When Claude stops, the project moves to the top of the live group and tmux
shows a message. `ctx run -- just test` does the same for a long command.

On the project screen, `w` with the cursor on a PR, MR or work item starts the list at that item's
first wait, such as its review.

### Notes

- `c` in ctx, or `ctx clip`, saves the clipboard as a note and asks for a title and source.
- `N`, or `ctx note`, opens `$EDITOR` on a new note.
- `I`, or `ctx import <link or words>`, asks Claude to find a Teams thread or email through the
  Microsoft 365 connector and saves a summary with the source link. Claude gets only the tools in
  `tools` under `[import]`, which default to the connector's read tools. An empty list turns
  import off.
- `ctx link <path>` adds a local file, such as a plan, which is indexed where it is.

`/` searches notes, handoffs, the journal and item text. Tab switches between the current project
and all projects.

### Claude

A Claude session belongs to the project whose tmux session it runs in. ctx sets `CTX_PROJECT` on
each project's session, every window and pane in it inherits the variable, and the hooks from
`ctx hooks` use it to report to the right project. A `claude` started by hand in any window of
the session counts, and so does one started with `C`. `C` runs `command` from the `[claude]`
section, `claude` by default. When `config_dir` is set, ctx sets `CLAUDE_CONFIG_DIR` to it, so a
shell alias is not needed. The project screen lists recent sessions that Claude kept a
transcript for. Enter on one goes to its window while it still runs, and otherwise runs
`claude --resume` in a new window.

When Claude works in a linked git worktree, the hooks add the worktree's branch to the project,
stored against the repository's main checkout. ctx checks at the start of each session and at the
end of each turn, and skips a branch the project already holds and a detached HEAD. With the
branch in the project, `r` on its PR or MR uses Claude's worktree as the checkout, `C` starts the
fixes there, and the branch's PR or MR shows up as a suggestion after the next refresh.

After a freeze, Claude writes a short summary of where the work stands into the journal. When you
thaw a project with several changes waiting, Claude writes a brief that appears at the top of the
project screen. Both run in the background and never hold up a switch. They use `summary_model`
in the `[claude]` section of the config, Haiku by default, and `ctx import` uses `import_model`,
Sonnet by default. `summarize_on_freeze` turns the summaries off.

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
the work item of a listed PR. When none does either, the entry goes to the project of the
current session, and the items are added to it. `--add` adds items Claude created, such as a new
PR for a listed work item, to each project the entry went to. `--message -` reads the summary
from stdin, and `--project` names the project instead of searching.

## Files

```text
~/.config/ctx/config.toml
~/.local/share/ctx/
  projects/<slug>/
    project.toml      name, state, tmux session and layout, items, wait condition
    journal.md        handoffs, state changes and Claude summaries, newest last
    snapshot.json     item states at the last freeze
    visit.json        item states when you last left the project
    events.jsonl      Claude and ctx run events, for wait conditions
    brief.json        the last Claude brief
    responses/        Claude's responses to PR and MR threads, one file per item
    notes/<date>-<title>.md
  cache.json          the latest state of every item
  inbox.json          your PRs, MRs and work items, for the Unfiled section
  index.db            the search index; ctx reindex rebuilds it
```

The `projects` directory is a git repository, and ctx commits after each freeze and state change.
`CTX_HOME` and `CTX_CONFIG_HOME` move the two directories, for testing.
