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

Invoke-Step 'rustfmt (check)' { cargo fmt --all -- --check }
Invoke-Step 'clippy (deny warnings)' { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'build' { cargo check --workspace }
Invoke-Step 'tests' { cargo test --workspace }

Write-Host 'All checks passed.'
