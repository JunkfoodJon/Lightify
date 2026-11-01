<#
.SYNOPSIS
  Build the release binary and package it as the Lightify installer.

.DESCRIPTION
  One command from a clean checkout to a shippable Setup .exe:

      pwsh -File scripts\build-installer.ps1

  Steps: cargo build --release of the shell (which embeds the icon + version
  resource via build.rs), the downloader bridge, and lightify-installer (the themed
  setup / updater / uninstaller stub and its lightify-pack tool); then lightify-pack
  appends the zstd-compressed payload (Lightify.exe, bridge, ffmpeg, LICENSE.md) to
  the stub, producing target\installer\Lightify-Setup-<version>.exe.

  Inno Setup is no longer used (installer\lightify.iss is kept for reference only).
  scripts\stage-release.ps1 signs the result (latest.json) for auto-update.

  Payload is opt-out, not opt-in: a release build is meant to work on someone
  else's machine, and the downloader is useless without its bridge and ffmpeg.
  Pass -NoDownloader / -NoFfmpeg to leave them out (much smaller installer).

.NOTES
  ffmpeg is GPL/LGPL. Bundling it means shipping its licence text and being able
  to supply its source on request - see LICENSE.md.
#>
[CmdletBinding()]
param(
    # Skip the OnTheSpot bridge (~47 MB). Downloads then report a missing bridge.
    [switch]$NoDownloader,
    # Skip ffmpeg (~202 MB). The bridge falls back to an ffmpeg on PATH.
    [switch]$NoFfmpeg,
    # Package whatever is already built (shell, bridge, installer stub) instead of rebuilding.
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$shell = Split-Path -Parent $PSScriptRoot
$repo  = Split-Path -Parent $shell
$installerCrate = Join-Path $repo 'lightify-installer'

function Fail($msg) { Write-Error $msg; exit 1 }

# ── Version comes from Cargo.toml, so it can never drift from the binary ─────
$cargo = Get-Content (Join-Path $shell 'Cargo.toml') -Raw
if ($cargo -notmatch '(?m)^\s*version\s*=\s*"([^"]+)"') { Fail 'Could not read version from Cargo.toml' }
$version = $Matches[1]
Write-Host "Lightify $version" -ForegroundColor Cyan

# ── 1. Build ────────────────────────────────────────────────────────────────
if (-not $SkipBuild) {
    Write-Host 'Building release binary...' -ForegroundColor Cyan
    Push-Location $shell
    try {
        # An inherited CC short-circuits the cc crate's MSVC detection, which then
        # fails to set the SDK include paths for the C-backed dependencies.
        $savedCC = $env:CC
        Remove-Item Env:\CC -ErrorAction SilentlyContinue
        try { cargo build --release } finally { if ($savedCC) { $env:CC = $savedCC } }
        if ($LASTEXITCODE -ne 0) { Fail 'cargo build failed' }
    } finally { Pop-Location }

    # This crate is what actually (re)builds the bundled downloader bridge -
    # lightify-shell's own `cargo build` above never touches it. Skipping this step
    # used to mean whatever `lightify-onthespot.exe` happened to already be sitting
    # in that dist folder got bundled silently, stale fix or not - the check below
    # only ever verified the file *existed*, never that it was current. That gap is
    # exactly how a source fix here can fail to reach the shipped installer even
    # after "rebuilding" it.
    if (-not $NoDownloader) {
        Write-Host 'Building downloader bridge (lightify-tauri/src-tauri)...' -ForegroundColor Cyan
        Push-Location (Join-Path $repo 'lightify-tauri\src-tauri')
        try {
            $savedCC = $env:CC
            Remove-Item Env:\CC -ErrorAction SilentlyContinue
            try { cargo build --release } finally { if ($savedCC) { $env:CC = $savedCC } }
            if ($LASTEXITCODE -ne 0) { Fail 'cargo build failed for lightify-tauri\src-tauri (downloader bridge)' }
        } finally { Pop-Location }
    }

    # The installer stub (Lightify-Setup.exe) and its packer. Its build.rs reads the
    # version from lightify-shell/Cargo.toml, so the stub's file version matches.
    Write-Host 'Building installer (lightify-installer)...' -ForegroundColor Cyan
    Push-Location $installerCrate
    try {
        $savedCC = $env:CC
        Remove-Item Env:\CC -ErrorAction SilentlyContinue
        try { cargo build --release } finally { if ($savedCC) { $env:CC = $savedCC } }
        if ($LASTEXITCODE -ne 0) { Fail 'cargo build failed for lightify-installer' }
    } finally { Pop-Location }
}

$exe = Join-Path $shell 'target\release\Lightify.exe'
if (-not (Test-Path $exe)) { Fail "Missing $exe - build it first (drop -SkipBuild)." }
$stub = Join-Path $installerCrate 'target\release\Lightify-Setup.exe'
$pack = Join-Path $installerCrate 'target\release\lightify-pack.exe'
foreach ($p in $stub, $pack) {
    if (-not (Test-Path $p)) { Fail "Missing $p - build lightify-installer first (drop -SkipBuild)." }
}
# The pseudo language (LIGHTIFY_PSEUDO, LANGUAGE-PLAN.md Phase 1) is a test build only.
foreach ($p in $exe, $stub) {
    if (Select-String -Path $p -Pattern 'qps-ploc' -SimpleMatch -Quiet) {
        Fail "$p was built with LIGHTIFY_PSEUDO (pseudo language) - rebuild without it."
    }
}
$license = Join-Path $repo 'LICENSE.md'
if (-not (Test-Path $license)) { Fail "Missing $license" }

# ── 2. Payload checks, before packing ───────────────────────────────────────
$payload = @($exe)

if (-not $NoDownloader) {
    $bridge = Join-Path $repo 'lightify-tauri\src-tauri\target\pyinstaller\release\dist\lightify-onthespot.exe'
    if (-not (Test-Path $bridge)) {
        Fail "Downloader bridge not found at $bridge. Build it (cd lightify-tauri\src-tauri; cargo build --release), or pass -NoDownloader."
    }
    # Belt and braces, independent of -SkipBuild above (which a caller can pass even
    # when the bridge genuinely is stale): refuse to ship a bridge older than its own
    # source - see the comment on the build step above for why this check exists at all.
    $bridgeTime = (Get-Item $bridge).LastWriteTime
    $onthespotSrc = Join-Path $repo 'lightify-tauri\docs\onthespot-master\onthespot-master\src'
    if (Test-Path $onthespotSrc) {
        $newestSrc = Get-ChildItem $onthespotSrc -Recurse -File |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($newestSrc -and $newestSrc.LastWriteTime -gt $bridgeTime) {
            Fail ("Downloader bridge at $bridge ($bridgeTime) predates a newer source file " +
                  "($($newestSrc.FullName), $($newestSrc.LastWriteTime)) - rebuild it: " +
                  "cd lightify-tauri\src-tauri; cargo build --release")
        }
    }
    $bridgeHash = (Get-FileHash $bridge -Algorithm SHA256).Hash.Substring(0, 12).ToLower()
    Write-Host "Downloader bridge: $bridge" -ForegroundColor DarkGray
    Write-Host "  built $bridgeTime, sha256 $bridgeHash..." -ForegroundColor DarkGray
    $payload += $bridge
}
if (-not $NoFfmpeg) {
    $ffmpeg = Join-Path $repo 'lightify-tauri\src-tauri\target\release\ffmpeg.exe'
    if (Test-Path $ffmpeg) { $payload += $ffmpeg }
    else { Fail "ffmpeg not found at $ffmpeg. Provide it, or pass -NoFfmpeg." }
}
$payload += $license

# ── 3. Pack: the stub + the compressed payload ──────────────────────────────
# lightify-pack reads the result back the way the stub will before it reports success.
$outDir = Join-Path $shell 'target\installer'
$null = New-Item -ItemType Directory -Path $outDir -Force
$out = Join-Path $outDir "Lightify-Setup-$version.exe"
Write-Host "Packing with $pack" -ForegroundColor Cyan
& $pack --stub $stub --out $out --version $version @payload
if ($LASTEXITCODE -ne 0) { Fail 'lightify-pack failed' }
if (-not (Test-Path $out)) { Fail "lightify-pack reported success but $out is missing" }
$mb = [math]::Round((Get-Item $out).Length / 1MB, 1)
Write-Host "`nInstaller ready: $out ($mb MB)" -ForegroundColor Green
