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

.PARAMETER EnableCharge
  Also enable emerald charging: mint the backend's MC client cert (via the Hub's
  mc-pki CA) into app_backends/moymoy/mc-cert and set MOCHI_MC_CERT_DIR in the
  staged app.toml, so the backend connects to the command bus (can_charge=true).
  Without this the wallet deploys charge-DISABLED (MOCHI_MC_CERT_DIR unset →
  "チャージは現在利用できません"). NOTE: charge also needs the moymoy mod on the
  MC server + "moymoy" in mochi-server.toml [connector].hosted_app_ids.

.PARAMETER McCaDir
  The Hub's mc-pki CA directory (the leaf must chain to the CA the Hub trusts).
  Default: <MochiRepo>\.devstack\mc-pki\ca (the mochi-inworld devstack layout).

.EXAMPLE
  powershell -File tools/deploy-backend.ps1 -HubWorkdir D:\IdeaProjects\MochiOS2.0\.devstack\hub -EnableCharge
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $HubWorkdir,
    [switch] $NoBuild,
    [switch] $EnableCharge,
    [string] $MochiRepo = 'D:\IdeaProjects\MochiOS2.0',
    [string] $McCaDir
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'server\moymoy-cs\Cargo.toml'
if (-not $McCaDir) { $McCaDir = Join-Path $MochiRepo '.devstack\mc-pki\ca' }

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

# --- Emerald charge: mint the MC client cert + wire MOCHI_MC_CERT_DIR. --------
# Root cause of "チャージは現在利用できません": with no cert the backend never
# connects to the command bus, so can_charge=false. Minting the leaf here (signed
# by the Hub's CA) and setting MOCHI_MC_CERT_DIR flips it on.
if ($EnableCharge) {
    Write-Host "enabling emerald charge (minting MC client cert) ..." -ForegroundColor Cyan
    if (-not (Test-Path $McCaDir)) {
        throw "mc-pki CA dir not found: $McCaDir. Run the mochi-inworld devstack first (creates .devstack\mc-pki\ca), or pass -McCaDir <the Hub's CA dir>."
    }
    $mcCa = Join-Path $MochiRepo 'target\debug\mochi-mc-ca.exe'
    if (-not (Test-Path $mcCa)) {
        Write-Host "building mochi-mc-ca ..." -ForegroundColor DarkGray
        $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
        & cargo build -p mochi-hub-mc-pki --bin mochi-mc-ca --manifest-path (Join-Path $MochiRepo 'Cargo.toml')
        $code = $LASTEXITCODE; $ErrorActionPreference = $prev
        if ($code -ne 0 -or -not (Test-Path $mcCa)) { throw "mochi-mc-ca build failed (exit $code)" }
    }
    $certDir = Join-Path $dest 'mc-cert'
    # Client leaf bound to app_id 'moymoy' (the mod's ALLOWED_SRC), signed by the
    # Hub's CA (--flat, matching the devstack). Yields chain.pem/leaf.key.pem/ca.cert.pem.
    $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    & $mcCa issue --dir $McCaDir --mcserver-id moymoy --out $certDir --flat | Out-Null
    $code = $LASTEXITCODE; $ErrorActionPreference = $prev
    if ($code -ne 0) { throw "mochi-mc-ca issue failed (exit $code)" }
    foreach ($f in 'chain.pem', 'leaf.key.pem', 'ca.cert.pem') {
        if (-not (Test-Path (Join-Path $certDir $f))) { throw "cert missing after issue: $f" }
    }
    # Set MOCHI_MC_CERT_DIR in the staged app.toml (relative to the backend's
    # workdir = app_backends/moymoy). Uncomment the template line, else append.
    $toml = Get-Content $tomlDest -Raw
    if ($toml -match '(?m)^\s*#\s*MOCHI_MC_CERT_DIR\s*=') {
        $toml = $toml -replace '(?m)^\s*#\s*MOCHI_MC_CERT_DIR\s*=.*$', 'MOCHI_MC_CERT_DIR  = "mc-cert"'
    } elseif ($toml -notmatch '(?m)^\s*MOCHI_MC_CERT_DIR\s*=') {
        $toml = $toml.TrimEnd() + "`r`nMOCHI_MC_CERT_DIR  = `"mc-cert`"`r`n"
    }
    [System.IO.File]::WriteAllText($tomlDest, $toml, (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "charge enabled: cert -> $certDir ; MOCHI_MC_CERT_DIR=mc-cert set in app.toml." -ForegroundColor Green
    Write-Host "  Restart the backend to reconnect; can_charge should become true." -ForegroundColor DarkGray
    Write-Host '  ALSO REQUIRED on the MC server: load the moymoy mod jar (mod/build/libs/moymoy-*.jar)' -ForegroundColor Yellow
    Write-Host '  next to the mochi connector mod, and set a non-empty mcserver_id in mochi-server.toml' -ForegroundColor Yellow
    Write-Host '  (hosted_app_ids config is deprecated — the connector auto-advertises moymoy). Restart the MC server.' -ForegroundColor Yellow
}

Write-Host "Deployed to $dest" -ForegroundColor Green
Write-Host "Note: exec in app.toml is ['./moymoy-cs'] — on Windows the launcher resolves moymoy-cs.exe." -ForegroundColor DarkGray
if (-not $EnableCharge) {
    Write-Host "Charge is DISABLED (no MC cert). Re-run with -EnableCharge to mint the cert + enable it." -ForegroundColor DarkGray
}
