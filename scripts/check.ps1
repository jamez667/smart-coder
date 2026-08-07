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
Invoke-Step 'clippy (craft-only)' {
    cargo clippy -p sc-win --all-targets --features craft-only -- -D warnings
}
Invoke-Step 'tests (craft-only)' { cargo test -p sc-win --features craft-only }
# Spec drift (spec 17): anchors that no longer resolve, assertions that are false.
# Deterministic and model-free, so it costs nothing to run every time. `unknown`
# never gates and an ungoverned crate only warns — this fails on BROKEN or STALE.
Invoke-Step 'spec traceability' { cargo run --quiet -p sc-cli -- trace --check }

Write-Host 'All checks passed.'
