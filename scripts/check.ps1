# Local CI check (Windows): run the same gates a contributor's change must pass
# before it lands. See CONTRIBUTING.md. Exits non-zero on the first failing gate.
#
# Usage:  ./scripts/check.ps1
$ErrorActionPreference = 'Stop'

Set-Location (Join-Path $PSScriptRoot '..')

function Invoke-Step($name, [scriptblock]$body) {
    Write-Host "==> $name"
    & $body
    if ($LASTEXITCODE -ne 0) { throw "$name failed (exit $LASTEXITCODE)" }
}

# `sc-core`'s TDD-loop tests drive a real red→green run over a sample repo whose
# contract test is `sh test.sh` — a POSIX shell is a genuine requirement of those
# tests, not an accident. Without one on PATH, `run_verification` can never go
# green and two tests fail in a way that looks like broken agent logic. Git for
# Windows ships one; add it rather than leaving the gate red.
if (-not (Get-Command sh -ErrorAction SilentlyContinue)) {
    $gitSh = 'C:\Program Files\Git\usr\bin'
    if (Test-Path (Join-Path $gitSh 'sh.exe')) {
        Write-Host "==> adding $gitSh to PATH (sc-core's TDD tests need a POSIX sh)"
        $env:PATH = "$gitSh;$env:PATH"
    } else {
        throw "No POSIX 'sh' on PATH. sc-core's TDD-loop tests need one (Git for Windows provides it at C:\Program Files\Git\usr\bin)."
    }
}

Invoke-Step 'rustfmt (check)' { cargo fmt --all -- --check }
Invoke-Step 'clippy (deny warnings)' { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'build' { cargo check --workspace }
Invoke-Step 'tests' { cargo test --workspace }
# The CRAFT-ONLY build (spec 21). A cargo feature is only compiled when something asks
# for it, so without these two gates the flag rots silently: nothing in the default
# build would notice a `cfg(feature = "craft-only")` block that stopped compiling, or a
# test whose assumptions the pinned mode invalidates.
#
# Built into a SEPARATE target dir. Cargo writes every feature variant of a binary to the
# same target\debug\sc-win.exe, so these steps used to leave a craft-only executable at the
# path a developer then launches -- an app with no Chat, no Claude panel and no backend
# badge, from a config that says `assistant`. That looked like a corrupted layout and was
# neither: it was the gate replacing the binary.
$CraftBase = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
$CraftTarget = Join-Path $CraftBase 'craft-only'
Invoke-Step 'clippy (craft-only)' {
    $env:CARGO_TARGET_DIR = $CraftTarget
    try { cargo clippy -p sc-win --all-targets --features craft-only -- -D warnings }
    finally { Remove-Item Env:CARGO_TARGET_DIR -EA SilentlyContinue }
}
Invoke-Step 'tests (craft-only)' {
    $env:CARGO_TARGET_DIR = $CraftTarget
    try { cargo test -p sc-win --features craft-only }
    finally { Remove-Item Env:CARGO_TARGET_DIR -EA SilentlyContinue }
}
# Spec drift (spec 17): anchors that no longer resolve, assertions that are false.
# Deterministic and model-free, so it costs nothing to run every time. `unknown`
# never gates and an ungoverned crate only warns — this fails on BROKEN or STALE.
Invoke-Step 'spec traceability' { cargo run --quiet -p sc-cli -- trace --check }

Write-Host 'All checks passed.'
