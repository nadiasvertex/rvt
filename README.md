# rvt

rvt is a terminal board for open pull requests in Azure DevOps and merge requests in GitLab. It
lists ADO PRs whose linked work items fall under a configured area path, grouped by area, and
GitLab MRs in a configured group, grouped by project. You can use either forge or both. Enter opens a PR's description and comments in a second pane, and `r` runs a Claude review
whose findings you can check off for posting.

The repository is a Cargo workspace. `crates/rvt` is the board, `crates/ctx` is a tool for
switching between projects (see [its README](crates/ctx/README.md)), and `crates/forge` holds the
Azure DevOps, GitLab, git and Claude code they share. `crates/view` draws rvt's Markdown and
comment threads. `crates/ctx-core` holds ctx's projects and logic without a user interface, so the
ctx command line and the ctx app share them.

## Requirements

- For Azure DevOps, `az` with the `azure-devops` extension, logged in.
- For GitLab, `glab`, logged in to the configured GitLab host.
- `git` with SSH access to the configured hosts, and `claude`, for reviews.

## Build and run

`rust-toolchain.toml` pins the Rust version, and rustup installs it on the first build. The GUI
crates that the ctx app is being built on need Rust 1.89 or newer.

```sh
cargo build --release
./target/release/rvt
```

The first run writes `~/.config/rvt/config.toml` and stops until you fill in `[ado]`, `[gitlab]`,
or both. The board is cached, so later starts show the last
board at once and refresh it in the background. The board also refreshes itself every five
minutes. Set `refresh_minutes` under `[board]` in the config to change the interval, or set it to
`0` to turn the timed refresh off.

## Configuration

The config file is `~/.config/rvt/config.toml`. `RVT_CONFIG_HOME` points rvt at another directory.
Only the forge sections have no defaults.

| Setting | Meaning |
| --- | --- |
| `ado.organization` | The organization URL, such as `https://dev.azure.com/your-org`. |
| `ado.project` | The project that holds the repositories and work items. |
| `ado.area_root` | PRs are listed when a linked work item is under this area path. |
| `ado.lookback_days` | Work items changed within this many days are searched for PR links. `365` by default. |
| `ado.ssh_host` | The SSH host that repositories are cloned from. `ssh.dev.azure.com` by default. |
| `gitlab.host` | The GitLab host, such as `gitlab.example.com`. |
| `gitlab.group` | The group whose open MRs are listed. |
| `review.claude_command` | The program that runs Claude. `claude` by default. |
| `review.claude_config_dir` | The `CLAUDE_CONFIG_DIR` for reviews and diagnoses. Empty keeps your environment's. |
| `repo_areas` | Places a repository's PRs in an area when no linked work item names one. |

The other settings are described in the template that the first run writes.

## Keys

Press `?` in rvt for the full list and a legend of the markers at the end of each board row.

| Key | Action |
| --- | --- |
| `j` `k`, `gg` `G`, `Ctrl-d` `Ctrl-u` | Move |
| `za` or Space, `zM` `zR` | Fold a group or category, fold all, open all |
| `Enter` | Open details |
| `Tab`, `h` `l`, `J` `K` | Switch pane, scroll details |
| `u` | Only unresolved comments in details |
| `/`, `Esc` | Filter, clear |
| `m` | Only items I wrote or review |
| `w` | Only items waiting for review |
| `r` | Run a Claude review |
| `d` | Diagnose a failed GitLab CI run |
| `f` | Show findings, or switch back to details |
| Space, `a`, `1` `2` `3` | In findings: toggle one, all, or one severity |
| `p` | In findings: post the checked findings, after `y` confirms |
| `A` | Approve the open or selected PR or MR, after `y` confirms |
| `o` | Open in browser |
| `R` | Refresh |
| `q` | Quit |

## Details pane

The details pane renders the description, comments, and CI diagnoses as Markdown. Headings,
emphasis, inline code, links, lists, task lists, quotes, tables, and code blocks are styled for the
terminal. Diff and suggestion blocks color their changed lines.

Comments are grouped into threads. Each thread hangs from a gutter colored by its status: yellow for
open, magenta for pending, and green for resolved. Resolved threads are grayed out, and `u` hides
them. Each author keeps one color across threads, and your own comments are marked `(you)`. An open
thread ends with `↩ Your turn` when it waits on you, or `Waiting for a reply` when you wrote last.

## Top of the board

Some items move from their area or project group to the top of the board. rvt sorts them into two
categories, using the comment threads it fetches for items you wrote, items you review, and items
you posted findings to. Each category lists the item with the newest comment first.

- **Reviews Awaiting Approval** lists items you did not write and that are not approved. You must
  have commented on the item, and each thread you commented on is resolved or has the author's
  reply last.
- **PRs Awaiting Response** lists items you wrote that have an open thread with someone else's
  comment last.

Below them, **Needs me** lists each open thread where someone answered you.

rvt treats an item as approved based on its overall status. In GitLab, an MR that needs more
approvals than yours, or that has failing CI, stays in Reviews Awaiting Approval after you
approve it.

## How ADO PRs get an area

PRs have no area path of their own. rvt reads work items under `ado.area_root` that changed within
`ado.lookback_days` and have links, then takes the area of the work item that links each active PR.
A PR with no linked work item under the area root is left off the board unless its repository is listed in
`[repo_areas]`.

## Reviews

`r` reviews the selected item. rvt keeps a blobless bare clone of each repository under
`~/Library/Caches/rvt/repos/`, fetches the item's head and target branch, and checks out the head
commit in a temporary worktree. It runs `claude -p` there with read-only tools (Read, Grep, Glob,
and `git diff`, `git log`, and `git show`). The prompt is `~/.config/rvt/review-prompt.md`, which
rvt writes on first run, followed by the commits to compare and the output format.

Each finding has a severity, a file and line, a title, and a body. rvt checks the line against the
diff hunks. A finding inside a hunk can be posted on that line. Any other finding will be posted as
a PR comment that names its location. High and medium findings start checked.

`p` in the findings view posts the checked findings that have not been posted yet. rvt asks
for `y` first. It refuses a stale review, and it checks that the item's head commit still matches the
reviewed commit before it posts anything. An inline finding becomes a comment on its line: an ADO
thread on the right side of the diff, or a GitLab discussion on the new line. GitLab also needs the
old line for an unchanged context line, which rvt reads from the MR's diff. Other findings become PR
comments that name their location. Posted findings show `[✓]` and are skipped by later posts. If
some posts fail, the first error is shown and `p` retries only the ones that did not post.

Reviews are saved under `~/Library/Caches/rvt/reviews/` and survive a restart. A review whose item
has new commits shows as stale. At most `review.max_parallel` reviews run at once, and the rest
wait in a queue. On the board, a row ends with a spinner while its review runs and `R` once it is
done, colored by its worst finding.

Reviews run `review.claude_command`. When `review.claude_config_dir` is set, rvt sets
`CLAUDE_CONFIG_DIR` to it, because a shell alias that sets the variable does not reach rvt's child
processes. `review.model` defaults to `opus`. Set it to another alias or model name, or leave it empty for the `claude` default.

## CI failures

A GitLab MR shows `CI failed` when its head pipeline has failed, and `CI running` while the head
pipeline is running. rvt reads the pipeline itself, because GitLab's merge status can still read
`checking` or `mergeable` after a failure. A merge conflict is shown before CI.

`d` diagnoses a failed MR. rvt reads the logs of the failed jobs, following child pipelines, and
keeps the last lines of each. `diagnose.triage_model` (`haiku` by default) sorts the failure as
either trivial, such as lint, a build error, or an infrastructure problem, or as a test failure.
A trivial failure shows a one-line summary. A test failure gets a checkout of the head commit, and
`diagnose.fix_model` (`sonnet` by default) reads the code and gives a cause and a fix. The
diagnosis is saved under `~/Library/Caches/rvt/diagnoses/`, and it is marked stale when the MR has
new commits. Diagnoses run under the same Claude setup as reviews. Only GitLab MRs can be
diagnosed.

## Tests

```sh
cargo test
cargo test live -- --ignored --nocapture   # renders the live board as text
```

The live test reads `RVT_W`, `RVT_H`, and `RVT_KEYS` to set the screen size and replay keys.
`RVT_REVIEW=gitlab-parser-1402 cargo test live_review -- --ignored --nocapture` runs one real
review and prints the findings. `RVT_DIAGNOSE=gitlab-parser-1366 cargo test live_diagnose --
--ignored --nocapture` runs one real CI diagnosis and prints the detail pane.

## License

rvt and ctx are released under the [MIT License](LICENSE).
