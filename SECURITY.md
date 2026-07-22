# Security Policy

`smart-coder` runs a language model that reads and **writes code and executes
commands** on your machine. Treat any run as you would running code you don't
fully trust: the model operates inside a permission layer and a sandbox (see
[spec 04 — Tools](docs/specs/04-tools.md)), but you are ultimately responsible
for what you point it at.

## Reporting a vulnerability

Please **do not** open a public issue for security problems.

Report privately via GitHub's
[private vulnerability reporting](https://github.com/jamez667/smart-coder/security/advisories/new)
("Report a vulnerability" on the repo's Security tab). If that's unavailable,
open a minimal public issue asking for a private contact — without details.

Please include: what the issue is, how to reproduce it, and the impact you
foresee. We'll acknowledge as soon as we reasonably can and keep you posted on a
fix.

### Especially interested in

- Ways the **permission layer** or **verify sandbox** can be bypassed so the
  agent writes outside the workspace, runs unapproved commands, or edits frozen
  contract tests.
- **Secret exposure** — anything that could cause `.env` / API keys / TLS keys to
  be logged, transmitted, or committed.
- Prompt-injection paths where repository content can steer the agent into
  unsafe tool calls.

## Handling secrets

Local secrets (`.env`, `*.ts.net.crt`, `*.ts.net.key`) are gitignored and must
never be committed. If you discover a secret committed to history, report it
privately as above so it can be rotated and purged.
