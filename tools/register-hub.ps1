<#
.SYNOPSIS
  Register (or re-register) the MoyMoy app in the running MochiOS HUB's App
  registry — one deterministic command.

.DESCRIPTION
  The dev HUB's app-registry is IN-MEMORY (DEV profile), so every HUB restart
  wipes the catalog and the app must be re-registered. This script does the whole
  flow in one shot:
    1. Ensure mochi-app-pack.exe exists (build it robustly — the upstream
       mochi-publish-app.ps1 rebuild path breaks under Windows PowerShell 5.1's
       native-stderr wrapping, so we pre-build here).
    2. Mint a session token: register a dev account (tolerating a conflict) + login.
    3. Publish: pack the bundle, POST the manifest to the registry, PUT the
       tar + icon to the repository (via tools/publish-moymoy.ps1).

  Run this after every HUB restart, or whenever `GET :7405/apps` no longer lists
  com.mochi.moymoy.

.EXAMPLE
  powershell -File tools/register-hub.ps1
#>
[CmdletBinding()]
param(
    [string] $Email = 'moymoy-dev@example.com',
    [string] $Password = 'moymoy-dev-pw-12345',
    [string] $MochiRepo = 'D:\IdeaProjects\MochiOS2.0',
    [string] $AccountUrl = 'http://127.0.0.1:7403',
    [string] $AuthUrl = 'http://127.0.0.1:7402',
    [string] $RegistryUrl = 'http://127.0.0.1:7405',
    [string] $RepositoryUrl = 'http://127.0.0.1:7409'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# --- 1. Ensure mochi-app-pack.exe exists (pre-build to dodge the 5.1 stderr trap).
$packExe = Join-Path $MochiRepo 'target\debug\mochi-app-pack.exe'
if (-not (Test-Path $packExe)) {
    Write-Host "building mochi-app-pack ..." -ForegroundColor Cyan
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'  # cargo writes progress to stderr
    & cargo build -p mochi-app-pack --manifest-path (Join-Path $MochiRepo 'Cargo.toml')
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    if ($code -ne 0 -or -not (Test-Path $packExe)) { throw "mochi-app-pack build failed (exit $code)" }
}

# --- 2. Mint a session token (register dev account if needed, then login).
try {
    Invoke-RestMethod -Method Post -Uri "$AccountUrl/accounts" -ContentType 'application/json' `
        -Body (@{ email = $Email; password = $Password } | ConvertTo-Json) | Out-Null
    Write-Host "account registered ($Email)" -ForegroundColor DarkGray
} catch {
    Write-Host "account exists or registration skipped - continuing" -ForegroundColor DarkGray
}
$login = Invoke-RestMethod -Method Post -Uri "$AuthUrl/auth/login" -ContentType 'application/json' `
    -Body (@{ email = $Email; password = $Password; device_id = [guid]::NewGuid().ToString() } | ConvertTo-Json)
$token = $login.access_token
if (-not $token) { throw "no access_token from $AuthUrl/auth/login" }
Write-Host "session token acquired" -ForegroundColor DarkGray

# --- 3. Publish (pack -> registry POST + repository PUT).
& (Join-Path $root 'tools\publish-moymoy.ps1') -Token $token `
    -MochiRepo $MochiRepo -RegistryUrl $RegistryUrl -RepositoryUrl $RepositoryUrl

# --- 4. Verify it stuck.
try {
    $apps = Invoke-RestMethod -Method Get -Uri "$RegistryUrl/apps" -TimeoutSec 8
    $ids = @($apps.apps | ForEach-Object { $_.id })
    if ($ids -contains 'com.mochi.moymoy') {
        Write-Host "verified: com.mochi.moymoy is registered ($RegistryUrl/apps)" -ForegroundColor Green
    } else {
        Write-Host "WARNING: com.mochi.moymoy not found in registry after publish" -ForegroundColor Yellow
    }
} catch {
    Write-Host "could not verify registry ($($_.Exception.Message))" -ForegroundColor Yellow
}
