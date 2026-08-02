<#
.SYNOPSIS
  Build the MoyMoy backend (release) and stage it into a Hub workdir's
  app_backends/moymoy/ directory.

.DESCRIPTION
  Builds server/moymoy-cs in release, then copies the binary + deploy/app.toml
  into <HubWorkdir>/app_backends/moymoy/. The launcher picks it up; enable it in
  the Hub TUI (or app.toml already sets enabled = true). Existing moymoy.db is
  preserved (never overwritten).

  Emerald charge needs NO deploy step here any more. It used to require an mTLS
  client certificate for the Hub's command bus (:7421), which this script minted
  under -EnableCharge; charging is now ordinary HTTP in MNN over the backend's
  own cs tunnel, so there is no second credential to issue. `can_charge` follows
  that tunnel's liveness. What charge still needs is on the Minecraft side: the
  moymoy mod jar loaded next to the mochi connector mod, on a server whose
  connector is configured.

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
    Write-Host "  NOTE: domain must be 'moymoy.cs.mnn'. An app.toml staged before the" -ForegroundColor Yellow
    Write-Host "  command-bus removal still says 'wallet.moymoy.cs.mnn' and will claim" -ForegroundColor Yellow
    Write-Host "  a host the app no longer talks to — update it by hand." -ForegroundColor Yellow
} else {
    Copy-Item (Join-Path $root 'deploy\app.toml') $tomlDest
    Write-Host "app.toml staged from deploy/app.toml (domain = moymoy.cs.mnn)." -ForegroundColor Yellow
}

Write-Host "Deployed to $dest" -ForegroundColor Green
Write-Host "Note: exec is auto-detected from this dir — on Windows the launcher resolves moymoy-cs.exe." -ForegroundColor DarkGray
Write-Host "Emerald charge is on whenever the cs tunnel is up; load the moymoy mod jar on the MC server to use it." -ForegroundColor DarkGray
