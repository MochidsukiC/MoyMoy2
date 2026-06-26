<#
.SYNOPSIS
  Publish the MoyMoy mobile app to the MochiOS external-app loader so the in-phone
  App Store (com.mochi.appstore) can install it (DEV.md §4.6).

.DESCRIPTION
  Wraps the MochiOS loader's publish tool: it packs app-mobile/apps/com.mochi.moymoy
  (minus manifest.json) into a deterministic tar, rewrites the manifest's
  bundle/icon sha256+size to the real bytes, POSTs the manifest to the App
  Registry, and PUTs the tar + icon to the App Repository.

.PARAMETER Token
  A bearer SESSION TOKEN from a logged-in dev session. With in-world OTP off you
  can mint one (open registration):
    $body = @{ email='you@example.com'; password='...' } | ConvertTo-Json
    Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7404/accounts -ContentType application/json -Body $body
    $tok = (Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7402/auth/login `
              -ContentType application/json `
              -Body (@{email='you@example.com';password='...';device_id='dev'}|ConvertTo-Json)).access_token

.EXAMPLE
  powershell -File tools/publish-moymoy.ps1 -Token $tok

  Prereqs: the devstack is running with the loader services — app-registry (:7405)
  + app-repository (:7409). (MochiOS2.0/tools/mochi-inworld.ps1.)
  Then in-world: App Store → install「MoyMoy」→ it appears on the home grid.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $Token,
    [string] $MochiRepo = 'D:\IdeaProjects\MochiOS2.0',
    [string] $RegistryUrl = 'http://127.0.0.1:7405',
    [string] $RepositoryUrl = 'http://127.0.0.1:7409'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$bundle = Join-Path $root 'app-mobile\apps\com.mochi.moymoy'
if (-not (Test-Path (Join-Path $bundle 'manifest.json'))) { throw "no manifest.json in $bundle" }
if (-not (Test-Path (Join-Path $bundle 'icon.png'))) { throw "no icon.png in $bundle (run tools/make-icon.mjs)" }

$publish = Join-Path $MochiRepo 'tools\mochi-publish-app.ps1'
if (-not (Test-Path $publish)) { throw "loader publish tool not found: $publish" }

& $publish -AppDir $bundle -Token $Token -RegistryUrl $RegistryUrl -RepositoryUrl $RepositoryUrl
Write-Host "MoyMoy published — install it from the in-phone App Store." -ForegroundColor Green
