# Build the plugins and install them where the app will find them (spec 25).
#
#     powershell -ExecutionPolicy Bypass -File scripts/install-plugins.ps1
#     ... -Product crafter        # install into smart-coder-crafter's state dir
#     ... -Product both           # both products
#     ... -NoBuild                # install what is already in target/release
#     ... -WhatIf                 # say what would happen, change nothing
#
# A plugin is a directory under <state dir>\plugins\ holding plugin.json and the
# binary. Nothing built that layout before this script: the release pipeline
# produces Linux tarballs, and on Windows the answer was "copy two files per
# plugin, four times, and get the directory names right".
#
# THE RULE THIS SCRIPT EXISTS TO KEEP: it never changes `enabled`.
#
# `plugin.json` carries an optional `enabled` flag that the Plugins panel writes
# when you toggle a plugin off. A reinstall that recreated the manifest from
# scratch would silently turn a disabled plugin back on -- and it would look like
# the toggle not persisting, not like the installer overwriting it. So an existing
# manifest keeps its own flag, and only the binary is replaced.

param(
    [ValidateSet('smart-coder', 'crafter', 'both')]
    [string]$Product = 'smart-coder',
    [switch]$NoBuild,
    [switch]$WhatIf
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

# The install directory is the plugin's identity until the handshake replaces it
# with the manifest's own id, so these names are load-bearing: they appear in the
# Plugins panel for a plugin that fails before it can say what it is called.
$Plugins = @(
    @{ Bin = 'sc-plugin-claude'; Dir = 'claude-code'; What = 'Claude Code' },
    @{ Bin = 'sc-plugin-agent';  Dir = 'agent';       What = 'The agent: chat, runs, the swarm, review gates' },
    @{ Bin = 'sc-plugin-comply'; Dir = 'compliance';  What = 'Compliance: the audit, and its optional prose' }
)

$StateDirs = switch ($Product) {
    'smart-coder' { , 'smart-coder' }
    'crafter'     { , 'smart-coder-crafter' }
    'both'        { 'smart-coder', 'smart-coder-crafter' }
}

if (-not $NoBuild) {
    Write-Host '==> building the plugins (release)'
    cargo build --release -p sc-plugin-claude -p sc-plugin-agent -p sc-plugin-comply
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
}

foreach ($state in $StateDirs) {
    $root = Join-Path $env:APPDATA "$state\plugins"
    Write-Host ''
    Write-Host "==> $state"

    foreach ($p in $Plugins) {
        $src = "target\release\$($p.Bin).exe"
        if (-not (Test-Path $src)) {
            throw "$src is missing. Run without -NoBuild, or build it first."
        }

        $dest = Join-Path $root $p.Dir
        $manifest = Join-Path $dest 'plugin.json'

        # Read the existing flag BEFORE touching anything. Absent means enabled --
        # the same default `parse_launch` applies, so a fresh install needs no flag
        # written at all.
        $keptEnabled = $null
        if (Test-Path $manifest) {
            try {
                $existing = Get-Content $manifest -Raw | ConvertFrom-Json
                if ($null -ne $existing.enabled) { $keptEnabled = [bool]$existing.enabled }
            } catch {
                Write-Host "    $($p.Dir): plugin.json is unreadable, rewriting it"
            }
        }

        $state_note = if ($keptEnabled -eq $false) { '  (kept disabled)' } else { '' }
        if ($WhatIf) {
            Write-Host "    would install $($p.Dir)$state_note  <- $src"
            continue
        }

        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Copy-Item $src (Join-Path $dest "$($p.Bin).exe") -Force

        # `.exe` on purpose. `resolve()` joins the plugin directory and checks the
        # file EXISTS before spawning; a bare name that fails that check falls back
        # to a PATH lookup, which finds nothing and reports "was not found".
        $obj = [ordered]@{ command = "$($p.Bin).exe" }
        if ($null -ne $keptEnabled) { $obj.enabled = $keptEnabled }
        # WriteAllText, NOT `Set-Content -Encoding UTF8`. On Windows PowerShell 5.1
        # that switch writes a UTF-8 BOM, and `parse_launch` hands the file straight
        # to `serde_json::from_str`, which rejects a leading BOM -- so every plugin
        # installed by the first version of this script was reported as "plugin.json
        # is not valid JSON" and refused at startup. WriteAllText emits no BOM on
        # either edition.
        [System.IO.File]::WriteAllText($manifest, ($obj | ConvertTo-Json), (New-Object System.Text.UTF8Encoding $false))

        Write-Host "    $($p.Dir)$state_note  -- $($p.What)"
    }
}

Write-Host ''
if ($WhatIf) {
    Write-Host 'Nothing was changed (-WhatIf).'
} else {
    # Plugins are discovered at startup (spec 25): there is no hot-swap, and the
    # panel registry is built once before any layout is read.
    Write-Host 'Installed. Restart the app -- plugins load at startup.'
}
