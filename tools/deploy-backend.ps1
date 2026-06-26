<#
.SYNOPSIS
  Build the MoyMoy backend (release) and stage it into a Hub workdir's
  app_backends/moymoy/ directory.

.DESCRIPTION
  Builds server/moymoy-cs in release, then copies the binary + deploy/app.toml
  into <HubWorkdir>/app_backends/moymoy/. The launcher picks it up; enable it in
  the Hub TUI (or app.toml already sets enabled = true). Existing moymoy.db is
  preserved (never overwritten).

.PARAMETER HubWorkdir
  The Hub's working directory (the parent of app_backends/). Required.

.EXAMPLE
  powershell -File tools/deploy-backend.ps1 -HubWorkdir D:\IdeaProjects\MochiOS2.0\.devstack\hub
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $HubWorkdir,
    [switch] $NoBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'server\moymoy-cs\Cargo.toml'

if (-not $NoBuild) {
    Write-Host "cargo build --release ..." -ForegroundColor Cyan
    & cargo build --release --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}

$bin = Join-Path $root 'server\moymoy-cs\target\release\moymoy-cs.exe'
if (-not (Test-Path $bin)) { throw "release binary not found: $bin" }

$dest = Join-Path $HubWorkdir 'app_backends\moymoy'
New-Item -ItemType Directory -Force $dest | Out-Null

Copy-Item $bin (Join-Path $dest 'moymoy-cs.exe') -Force

# app.toml: don't clobber an operator-edited one (which may hold secrets).
$tomlDest = Join-Path $dest 'app.toml'
if (Test-Path $tomlDest) {
    Write-Host "app.toml exists — left as-is (edit it for secrets/overrides)." -ForegroundColor Yellow
} else {
    Copy-Item (Join-Path $root 'deploy\app.toml') $tomlDest
    Write-Host "app.toml staged from deploy/app.toml — set MOCHI_TUNNEL_BEARER (+ MOCHI_MC_CERT_DIR for charge)." -ForegroundColor Yellow
}

Write-Host "Deployed to $dest" -ForegroundColor Green
Write-Host "Note: exec in app.toml is ['./moymoy-cs'] — on Windows the launcher resolves moymoy-cs.exe." -ForegroundColor DarkGray
