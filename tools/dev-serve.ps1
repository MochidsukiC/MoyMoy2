<#
.SYNOPSIS
  Static file server for the MoyMoy app bundle (browser-dev).

.DESCRIPTION
  Serves app-mobile/apps/com.mochi.moymoy over HTTP with permissive CORS, serving
  .jsx as application/javascript so Babel-standalone can load the components.
  Open the dev harness against a running backend (tools/run-cs.ps1):

    http://127.0.0.1:8099/dev.html?moymoy_http=http://127.0.0.1:7433&mcid=Steve

  Self-contained (System.Net.HttpListener) — no external dependency.
#>
[CmdletBinding()]
param(
    [int] $Port = 8099,
    [string] $Root = "$PSScriptRoot\..\app-mobile\apps\com.mochi.moymoy"
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path $Root).Path

$mime = @{
    '.html' = 'text/html; charset=utf-8'
    '.js'   = 'application/javascript; charset=utf-8'
    '.jsx'  = 'application/javascript; charset=utf-8'
    '.mjs'  = 'application/javascript; charset=utf-8'
    '.css'  = 'text/css; charset=utf-8'
    '.json' = 'application/json; charset=utf-8'
    '.png'  = 'image/png'
    '.svg'  = 'image/svg+xml'
    '.woff2' = 'font/woff2'
}

$listener = [System.Net.HttpListener]::new()
$listener.Prefixes.Add("http://127.0.0.1:$Port/")
$listener.Start()
Write-Host "MoyMoy dev server: http://127.0.0.1:$Port/ (root: $Root)" -ForegroundColor Green
Write-Host "  dev harness: http://127.0.0.1:$Port/dev.html?moymoy_http=http://127.0.0.1:7433&mcid=Steve" -ForegroundColor Cyan

try {
    while ($listener.IsListening) {
        $ctx = $listener.GetContext()
        $req = $ctx.Request
        $resp = $ctx.Response
        $resp.Headers.Add('Access-Control-Allow-Origin', '*')
        try {
            $rel = [System.Uri]::UnescapeDataString($req.Url.AbsolutePath).TrimStart('/')
            if ([string]::IsNullOrEmpty($rel)) { $rel = 'index.html' }
            $path = Join-Path $Root $rel
            # Prevent path traversal outside the bundle root.
            $full = [System.IO.Path]::GetFullPath($path)
            if (-not $full.StartsWith($Root, [System.StringComparison]::OrdinalIgnoreCase) -or -not (Test-Path $full -PathType Leaf)) {
                $resp.StatusCode = 404
                $resp.Close()
                continue
            }
            $ext = [System.IO.Path]::GetExtension($full).ToLowerInvariant()
            $resp.ContentType = if ($mime.ContainsKey($ext)) { $mime[$ext] } else { 'application/octet-stream' }
            $bytes = [System.IO.File]::ReadAllBytes($full)
            $resp.ContentLength64 = $bytes.Length
            $resp.OutputStream.Write($bytes, 0, $bytes.Length)
            $resp.Close()
        } catch {
            try { $resp.StatusCode = 500; $resp.Close() } catch {}
        }
    }
} finally {
    $listener.Stop()
}
