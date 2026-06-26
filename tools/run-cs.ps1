<#
.SYNOPSIS
  Run the MoyMoy wallet backend locally for development (no Hub required).

.DESCRIPTION
  Builds (unless -NoBuild) and runs server/moymoy-cs with TLS + the embedded
  tunnel DISABLED and the dev-credit endpoint ENABLED, so you can exercise the
  wallet over plain HTTP and fund a test account without the MNN overlay or the
  Minecraft mod. Pair it with the dev frontend:

    tools/run-cs.ps1                      # backend on http://127.0.0.1:7433
    tools/dev-serve.ps1                   # static server for the bundle (:8099)
    # browser: http://127.0.0.1:8099/dev.html?moymoy_http=http://127.0.0.1:7433&mcid=Steve

  Fund a test player:
    Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7433/wallet/_dev/credit `
      -ContentType application/json -Body (@{mcid='Steve';amount=12480}|ConvertTo-Json)
#>
[CmdletBinding()]
param(
    [string] $Listen = '127.0.0.1:7433',
    [string] $DbPath = "$PSScriptRoot\..\.devstack\moymoy-dev.db",
    [switch] $NoBuild,
    [switch] $Release
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'server\moymoy-cs\Cargo.toml'

if (-not $NoBuild) {
    $cargoArgs = @('build', '--manifest-path', $manifest)
    if ($Release) { $cargoArgs += '--release' }
    Write-Host "cargo $($cargoArgs -join ' ')" -ForegroundColor Cyan
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}

$profile = if ($Release) { 'release' } else { 'debug' }
$bin = Join-Path $root "server\moymoy-cs\target\$profile\moymoy-cs.exe"
if (-not (Test-Path $bin)) { throw "backend binary not found: $bin (build first)" }

$dbDir = Split-Path -Parent $DbPath
if (-not (Test-Path $dbDir)) { New-Item -ItemType Directory -Force $dbDir | Out-Null }

$env:MOCHI_APP_LISTEN = $Listen
$env:MOYMOY_CS_TLS = '0'        # plain HTTP for browser-dev
$env:MOYMOY_CS_TUNNEL = '0'     # no MNN overlay locally
$env:MOYMOY_DEV_CREDIT = '1'    # enable /wallet/_dev/credit (dev funding)
$env:MOYMOY_DB_PATH = $DbPath
if (-not $env:RUST_LOG) { $env:RUST_LOG = 'info' }

Write-Host "MoyMoy backend → http://$Listen  (db: $DbPath, TLS off, tunnel off, dev-credit on)" -ForegroundColor Green
& $bin
