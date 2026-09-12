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
# THE CRAFTER'S GUARANTEE (spec 21).
#
# `smart-coder-crafter` is an editor that cannot contact a language model, and the whole
# of that claim is its dependency tree: no crate that can reach a model is in it. This
# check is what keeps the claim true, and it replaced ~1,600 lines of tests that asserted
# each individual refusal at runtime -- a test proves a path was refused on the day it
# ran, a dependency tree proves the path does not exist.
#
# It fails the moment someone adds a model crate to `sc-crafter` or `sc-craft-ui`. That
# mistake is otherwise SILENT: the Crafter would still compile, still run, still look
# right, and no longer be what it says it is.
$Forbidden = @('sc-core', 'sc-model', 'sc-swarm', 'sc-workflow', 'sc-iterate',
               'sc-verify', 'sc-web', 'sc-proto', 'sc-comply', 'sc-tools')
Invoke-Step 'the crafter links no model code' {
    $tree = cargo tree -p sc-crafter --prefix none --no-dedupe 2>$null
    if (-not $tree) { throw "cargo tree -p sc-crafter produced nothing" }
    # Match the crate NAME at the start of a line, so a path containing the string
    # (or a crate that merely mentions one) cannot trip this.
    $names = $tree | ForEach-Object { ($_ -split ' ')[0] } | Where-Object { $_ }
    $found = $names | Where-Object { $Forbidden -contains $_ } | Sort-Object -Unique
    if ($found) {
        throw ("the Crafter must not link model code, but its tree contains: " +
               ($found -join ', ') +
               ". See spec 21 -- the editor half belongs in sc-craft-ui.")
    }
}

# Spec drift (spec 17): anchors that no longer resolve, assertions that are false.
# Deterministic and model-free, so it costs nothing to run every time. `unknown`
# never gates and an ungoverned crate only warns — this fails on BROKEN or STALE.
Invoke-Step 'spec traceability' { cargo run --quiet -p sc-cli -- trace --check }

Write-Host 'All checks passed.'
