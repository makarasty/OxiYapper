# Docs map

One line per doc: when to read it.

- `docs/SPEC.md`: before any change that touches behaviour; the owner's decisions, binding, wins every conflict. OPEN items are not decided yet.
- `docs/ARCHITECTURE.md`: before adding a crate, thread, IPC message, file on disk or pipeline step; section 13 for tests, section 14 for the CI gates.
- `docs/ROADMAP.md`: before starting work, to find the current milestone and its "done when" checks.
- `docs/SECRETS.md`: before touching a workflow, signing or anything that needs a token.
- `docs/research/stack-2026-11.md`: before adding or bumping a dependency; why each crate was picked and its known traps.
- `CONTRIBUTING.md`: before building, testing or committing; build prerequisites, the Windows target-dir rule, test commands, commit format.
- `SECURITY.md`: when you find or are told about a vulnerability; how to report it privately.
