---
name: ctx-update
description: Record finished work in the user's ctx project journal. Use after finishing a unit of work tied to a pull request, merge request, work item (ticket) or git branch, such as pushing commits, addressing review threads, finishing a review, or filing or updating a ticket, and right after creating a PR, MR or work item for an item already being worked on. Skip it for questions and reading that change nothing.
---

# Record work in ctx

The user tracks their projects with `ctx`. Each project holds PRs, MRs, work items and branches,
and keeps a journal. When you finish work on one of those items, run `ctx update` so the
project's journal says what was done. ctx finds the project that holds the item, or holds
something linked to it, so you don't need to know the project's name.

## When

Run it once at the end of each unit of work, not after every step:

- You pushed commits to a branch or a PR.
- You addressed or replied to review threads.
- You finished reviewing a PR or MR.
- You filed, updated or closed a work item.
- You created a PR, MR or work item for an item you were working on. Pass the new item with
  `--add` so the project picks it up.

## How

```sh
ctx update <item>... [--add <item>]... --message "<summary>"
```

Name each item as one shell argument:

| Item | Form |
| --- | --- |
| Azure DevOps PR | `pr:123` or its URL |
| Work item | `wi:456` or its URL (a bare `#456` starts a shell comment) |
| GitLab MR | `group/project!78`, `!78` inside the repository, or its URL |
| Current branch | `branch` (run it from the repository) |
| Another branch | `branch:name` |

Pass every item the work touched. Use `--add` only for items you created, or items the user
asked to have tracked with this work.

Write the summary in two to four plain sentences: what changed, and what is left or what the
work waits on. Name files, commits or tests only when they matter to whoever picks the work up
next. For a longer summary, pass `--message -` and send the text on stdin.

```sh
ctx update pr:4711 --message "Addressed the three review threads on null handling. \
Two threads are resolved and one waits on the reviewer's answer about the retry limit."

ctx update wi:98123 --add pr:4720 --message "Opened PR 4720 with the parser fix for this bug. \
CI is running."
```

## Afterwards

ctx prints the projects it updated and the items it added. Mention them to the user in one line.

If `ctx` is not installed, or it reports that no project matched, tell the user in one line and
carry on. Don't retry with other items or guess a project name.
