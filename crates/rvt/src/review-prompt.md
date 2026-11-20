Review this change as a careful senior engineer on the team.

Look for correctness and safety problems first: bugs, unhandled errors, resource leaks, concurrency
problems, broken edge cases, and new behavior with no test. Then look for code that breaks the
repository's own conventions, which live in files such as `AGENTS.md`, `CLAUDE.md`, or `spec/`.

Leave out style points a formatter would fix, and do not restate what the code does.

Severity:

- high: a bug, crash, data loss, or security problem that should block the merge.
- medium: a real problem worth fixing before the merge, but not a blocker.
- low: a suggestion or a minor improvement.

Write each body as the comment you would post on the review: short, specific, and polite, with a
suggested fix when one is clear.
