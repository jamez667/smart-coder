# Contributing to smart-coder

Thanks for your interest in `smart-coder` — an agentic coding tool built to run
on small, local language models, where the *harness* does the heavy lifting. See
the [README](README.md) for the project's goals and the
[specs](docs/specs/00-overview.md) for the design.

## Getting set up

You need a recent stable Rust toolchain (edition 2021). Install via
[rustup](https://rustup.rs), then:

```sh
rustup component add rustfmt clippy
cargo check --workspace
cargo test --workspace
```

The workspace is a set of `sc-*` crates under [`crates/`](crates/). Start with
[spec 01 — Architecture](docs/specs/01-architecture.md) to see how they fit
together.

Running the actual agent needs a model backend (Ollama / llama.cpp / vLLM / any
OpenAI-compatible server) — see [Running the backends](README.md#running-the-backends).
The tests do **not** require a live backend; they use a `MockBackend` and are
fully deterministic.

## Before you open a PR

Run the same checks a review will expect. There's a script that runs all of them:

```sh
./scripts/check.sh      # Linux / macOS
./scripts/check.ps1     # Windows (PowerShell)
```

That runs, in order:

1. `cargo fmt --all -- --check` — formatting must be clean (default rustfmt).
2. `cargo clippy --workspace --all-targets -- -D warnings` — no clippy warnings.
3. `cargo check --workspace` — it builds.
4. `cargo test --workspace` — the suite is green.

Please keep all four passing. If a clippy lint is genuinely wrong for a case,
prefer a narrowly-scoped `#[allow(...)]` **with a comment explaining why** over a
blanket allow.

## Guidelines

- **Tests are the control system.** This project is TDD-first — see
  [spec 11 — Testing & TDD](docs/specs/11-testing-and-tdd.md). New behavior
  should come with a test; a bug fix should come with a test that fails before it
  and passes after.
- **Keep changes focused.** Smallest change that correctly does the job; touch
  only what's necessary.
- **Match the surrounding code** — naming, module layout, comment density.
- **Discuss large changes first.** For anything architectural, open an issue to
  align before writing a lot of code.

## Secrets — never commit them

The working tree may contain local, untracked secret files that are **already
gitignored** and must stay that way:

- `.env` (e.g. `GEMINI_API_KEY`) — copy [`.env.example`](.env.example) to `.env`
  and fill it in locally.
- `*.ts.net.crt` / `*.ts.net.key` (local Tailscale TLS material).

Never `git add -f` these, and never paste an API key into tracked source, tests,
or docs. If you find a committed secret, report it privately (see
[SECURITY.md](SECURITY.md)).

## Reporting bugs & requesting features

Open a GitHub issue. For bugs, include what you ran, what you expected, what
happened, and — if the agent was involved — the model/backend and a run log if
you have one.

By contributing, you agree that your contributions are licensed under the
project's [MIT License](LICENSE).
