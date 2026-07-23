<#
.SYNOPSIS
  aish installer for Windows (x86_64 / ARM64).

.DESCRIPTION
  Downloads the aish release binary for this machine's architecture, verifies its
  SHA-256 (via the built-in Get-FileHash — no extra tools required), installs it to
  %LOCALAPPDATA%\Programs\aish, and adds that directory to the user PATH.

  By default it installs the newest release, including prereleases (GitHub
  /releases[0]). Use -Stable to install the latest stable ("Latest" badge) release
  (GitHub /releases/latest) instead.

.PARAMETER Stable
  Install the latest stable release instead of the newest release including
  prereleases.

.EXAMPLE
  irm https://raw.githubusercontent.com/tryandhappy/aish/main/install.ps1 | iex

.EXAMPLE
  .\install.ps1 -Stable
#>
[CmdletBinding()]
param(
    [switch]$Stable
)

$ErrorActionPreference = 'Stop'
$repo = 'tryandhappy/aish'
$ua = @{ 'User-Agent' = 'aish-installer' }

# 1. Architecture. PROCESSOR_ARCHITEW6432 is set when a 32/64-bit process runs on
#    an ARM64 host under emulation, so it takes precedence over PROCESSOR_ARCHITECTURE.
$procArch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$arch = if ($procArch -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$asset = "aish-$arch-pc-windows-msvc.exe"

# 2. Resolve the release tag (channel-aware; mirrors `aish --update` --stable/--prerelease).
if ($Stable) {
    $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers $ua
    $channel = 'stable'
} else {
    $rel = (Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases" -Headers $ua)[0]
    $channel = 'latest (incl. prerelease)'
}
$tag = $rel.tag_name
if (-not $tag) { throw "Could not determine the release tag from GitHub." }
Write-Host "Installing aish $tag ($channel, $arch) ..."

$base = "https://github.com/$repo/releases/download/$tag"

# 3. Download the binary to a temp file.
$tmp = Join-Path $env:TEMP ("aish-" + [System.Guid]::NewGuid().ToString('N') + ".exe")
try {
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $tmp -Headers $ua -UseBasicParsing
} catch {
    throw "Failed to download $asset for $tag. This release may not include a Windows binary (e.g. stable releases before Windows support). $($_.Exception.Message)"
}

# 4. Verify SHA-256. The .sha256 file is `<64-hex>  <filename>`; take the first token.
try {
    # GitHub serves .sha256 as octet-stream, so .Content may be a Byte[] rather than a
    # string (then .Trim() fails). Decode bytes to text when needed.
    $shaRaw = (Invoke-WebRequest -Uri "$base/$asset.sha256" -Headers $ua -UseBasicParsing).Content
    $shaText = if ($shaRaw -is [byte[]]) { [System.Text.Encoding]::UTF8.GetString($shaRaw) } else { [string]$shaRaw }
    $expected = (($shaText.Trim() -split '\s+')[0]).ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 -Path $tmp).Hash.ToLower()
    if ($expected -ne $actual) {
        throw "SHA-256 mismatch: expected $expected but got $actual"
    }
    Write-Host "SHA-256 verified."
} catch {
    Remove-Item $tmp -Force -ErrorAction SilentlyContinue
    throw "Checksum verification failed: $($_.Exception.Message)"
}

# 5. Install to %LOCALAPPDATA%\Programs\aish.
#    Move-Item -Force can throw ERROR_ALREADY_EXISTS overwriting an existing aish.exe,
#    and a running aish.exe can't be overwritten at all. Renaming a locked exe IS allowed
#    on Windows, so move the old one aside first, then drop the new one in.
#    The aside name must be UNIQUE: a leftover/locked "aish.exe.old" from a previous run
#    would otherwise block Rename-Item (which, even with -Force, does NOT overwrite an
#    existing target and can't delete a locked one). Then best-effort delete every stale
#    *.old (skips ones still locked/running — harmless leftovers).
$dir = Join-Path $env:LOCALAPPDATA 'Programs\aish'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$dest = Join-Path $dir 'aish.exe'
if (Test-Path $dest) {
    $aside = 'aish.exe.' + [System.IO.Path]::GetRandomFileName() + '.old'
    Rename-Item -Path $dest -NewName $aside -Force
}
Move-Item -Path $tmp -Destination $dest -Force
Get-ChildItem -Path $dir -Filter 'aish.exe.*.old' -ErrorAction SilentlyContinue |
    Remove-Item -Force -ErrorAction SilentlyContinue

# 6. Add the install dir to the user PATH (idempotent).
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath -notlike "*$dir*") {
    $newPath = if ([string]::IsNullOrEmpty($userPath)) { $dir } else { "$userPath;$dir" }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "Added $dir to your user PATH."
}

Write-Host ""
Write-Host "aish $tag installed to $dest"
Write-Host "Open a new terminal, then run:  aish"
