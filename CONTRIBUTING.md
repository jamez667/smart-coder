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

## The desktop app, and its plugins

Two binaries come out of `sc-win`, sharing one editor. The difference is one call
to `set_product` at the top of `main`: which state directory, which default
layout, which plugin directory, which name in the title bar.

| Binary | State dir |
| --- | --- |
| `smart-coder` | `%APPDATA%\smart-coder\` |
| `smart-coder-crafter` | `%APPDATA%\smart-coder-crafter\` |

`smart-coder-crafter` is the editor alone (spec 21). Its dependency tree contains
no crate that can reach a model, and `scripts/check.*` asserts that with
`cargo tree` — that assertion *is* the guarantee, which is why it is a gate and
not a comment.

The agent, Claude Code and compliance are **plugins** (spec 25): child processes
speaking line-delimited JSON, not code either binary links. A plugin is a
directory under `<state dir>\plugins\` holding `plugin.json` and its binary.

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install-plugins.ps1
# -Product crafter | both     which state dir(s)
# -NoBuild                    install what is already in target/release
# -WhatIf                     say what would happen, change nothing
```

Two things that script is careful about, because both are easy to get wrong by
hand:

* **It never changes a plugin's `enabled` flag.** The Plugins panel writes that
  when you toggle one off; a reinstall that recreated the manifest would silently
  turn it back on, and that would look like the toggle not persisting.
* **It writes `plugin.json` without a UTF-8 BOM.** `parse_launch` hands the file
  to `serde_json::from_str`, which rejects one — the plugin then shows up as
  "plugin.json is not valid JSON". Windows PowerShell 5.1's
  `Set-Content -Encoding UTF8` writes a BOM, so the script uses
  `[System.IO.File]::WriteAllText` instead.

Plugins are discovered at startup, so installing or toggling one needs a restart.
There is no hot-swap, for the reason spec 25 gives: the panel registry is built
once, before any layout is read.

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

The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml), so your
local `cargo clippy` uses the exact same compiler and lint set as CI — "passes
locally" means "passes CI". CI runs the same four gates via
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) on every push and PR to
`main`, plus a second, non-blocking `reports` job for the slow suites: retrieval
ranking, the gateway benchmark, and spec-anchor drift. Those catch real
regressions but none is a reason to refuse a merge on its own.

## Cutting a release

Releases are built and published by
[`.github/workflows/release.yml`](.github/workflows/release.yml) when a version
tag is pushed:

```sh
git tag v0.1.0
git push origin v0.1.0
```

That builds **five binaries** — both desktop products (`smart-coder` and
`smart-coder-crafter`, two `[[bin]]`s over one editor) and the three plugins —
and publishes each as its own archive:

| Platform | Asset |
| --- | --- |
| Linux | `<binary>-<tag>-linux-x86_64.tar.gz` |
| Windows | `<binary>-<tag>-windows-x86_64.zip` |

The Linux binaries are dynamically linked, so their runtime library deps are
listed in the archive's `README.txt`. The Windows binaries are **statically**
linked (see [`.cargo/config.toml`](.cargo/config.toml)) and need no such note.

Only the **3 newest** releases are kept — older ones, and their tags, are pruned
automatically to save space.

No secret is needed for the release itself: the built-in `GITHUB_TOKEN` covers
both GitHub Releases and the `sc-server` container push to GHCR. The one
optional repo secret is `SC_SERVER_PORTAINER_WEBHOOK`, which redeploys the
server stack after a tag; without it that step logs why and exits 0, because the
image is already published by the time it runs.

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
