<#
.SYNOPSIS
  Install the Knowell `know` command from a GitHub release (Windows).

.DESCRIPTION
  Downloads the release archive for this machine (x64 or ARM64), verifies it against the
  release's SHA256SUMS, installs know.exe into %LOCALAPPDATA%\Programs\knowell and adds that
  directory to your user PATH. Needs no administrator rights.

  One-liner:       irm https://raw.githubusercontent.com/knowell-dev/knowell/main/scripts/install.ps1 | iex
  Pinned version:  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/knowell-dev/knowell/main/scripts/install.ps1))) -Version 1.0.0
  Or set $env:KNOWELL_VERSION before the one-liner.

.PARAMETER Version
  Version to install, for example 1.0.0 (default: the latest release).

.PARAMETER InstallDir
  Install directory (default: $env:KNOWELL_INSTALL_DIR or %LOCALAPPDATA%\Programs\knowell).

.PARAMETER Attestation
  auto (default): verify build provenance when an authenticated gh is available;
  require: fail without it; skip: never check.

.NOTES
  Environment, mainly for testing: KNOWELL_REPO (default knowell-dev/knowell),
  KNOWELL_DOWNLOAD_BASE (replaces https://github.com/<repo>/releases/download),
  KNOWELL_NO_PATH=1 (do not touch the user PATH).
#>
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '', Justification = 'Interactive installer: progress goes to the console.')]
[CmdletBinding()]
param(
    [string]$Version = $env:KNOWELL_VERSION,
    [string]$InstallDir = $env:KNOWELL_INSTALL_DIR,
    [ValidateSet('auto', 'require', 'skip')]
    [string]$Attestation = 'auto'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
# Windows PowerShell 5.1 defaults to old TLS versions; GitHub needs 1.2+.
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { Write-Verbose 'could not adjust the TLS protocol list' }
$ProgressPreference = 'SilentlyContinue'

function Fail([string]$Message) { throw "knowell install: $Message" }

$Repo = if ($env:KNOWELL_REPO) { $env:KNOWELL_REPO } else { 'knowell-dev/knowell' }

function Get-LatestVersion {
    $response = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" -Method Head -UseBasicParsing
    # Windows PowerShell exposes ResponseUri; PowerShell 7 exposes RequestMessage.RequestUri.
    $final = if ($response.BaseResponse.PSObject.Properties['ResponseUri']) {
        $response.BaseResponse.ResponseUri.AbsoluteUri
    } else {
        $response.BaseResponse.RequestMessage.RequestUri.AbsoluteUri
    }
    $tag = ($final -split '/')[-1]
    if ($tag -notmatch '^v\d') { Fail 'could not determine the latest release (is there one yet? use -Version)' }
    return $tag.Substring(1)
}

if (-not $Version) {
    Write-Host "Looking up the latest release of $Repo ..."
    $Version = Get-LatestVersion
}
$Version = $Version -replace '^v', ''
if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') { Fail "invalid version: $Version" }

# PROCESSOR_ARCHITEW6432 is set for a 32-bit process on a 64-bit OS.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ($arch) {
    'AMD64' { $target = 'x86_64-pc-windows-msvc' }
    'ARM64' { $target = 'aarch64-pc-windows-msvc' }
    default { Fail "unsupported CPU architecture: $arch" }
}

$name = "knowell-$Version-$target"
$archive = "$name.zip"
$baseUrl = if ($env:KNOWELL_DOWNLOAD_BASE) { $env:KNOWELL_DOWNLOAD_BASE } else { "https://github.com/$Repo/releases/download" }
$baseUrl = "$baseUrl/v$Version"

if (-not $InstallDir) {
    if (-not $env:LOCALAPPDATA) { Fail 'LOCALAPPDATA is not set; pass -InstallDir' }
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\knowell'
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("knowell-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading Knowell $Version for $target ..."
    try {
        Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile (Join-Path $tmp $archive) -UseBasicParsing
        Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') -UseBasicParsing
    } catch {
        Fail "download failed from $baseUrl (is $Version released for $target?): $($_.Exception.Message)"
    }

    $expected = $null
    foreach ($line in Get-Content -LiteralPath (Join-Path $tmp 'SHA256SUMS')) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $archive) { $expected = $parts[0].ToLowerInvariant(); break }
    }
    if (-not $expected) { Fail "SHA256SUMS has no entry for $archive" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $tmp $archive)).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { Fail "checksum mismatch for $archive (expected $expected, got $actual); nothing was installed" }
    Write-Host 'Checksum verified.'

    if ($Attestation -ne 'skip') {
        $gh = Get-Command gh -ErrorAction SilentlyContinue
        $loggedIn = $false
        if ($gh) { & gh auth status *> $null; $loggedIn = ($LASTEXITCODE -eq 0) }
        if ($gh -and $loggedIn) {
            & gh attestation verify (Join-Path $tmp $archive) --repo $Repo *> $null
            if ($LASTEXITCODE -ne 0) { Fail "attestation verification failed for $archive; nothing was installed" }
            Write-Host 'Build provenance attestation verified.'
        } elseif ($Attestation -eq 'require') {
            Fail '-Attestation require needs the GitHub CLI (gh), logged in'
        } else {
            Write-Host 'Skipping attestation check (install and log in to gh to enable it).'
        }
    }

    Expand-Archive -LiteralPath (Join-Path $tmp $archive) -DestinationPath (Join-Path $tmp 'x') -Force
    $exe = Join-Path $tmp "x\$name\know.exe"
    if (-not (Test-Path -LiteralPath $exe)) { Fail "the archive does not contain $name\know.exe" }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    try {
        Copy-Item -LiteralPath $exe -Destination (Join-Path $InstallDir 'know.exe') -Force
    } catch {
        Fail "could not write $InstallDir\know.exe (is know running? close it and retry): $($_.Exception.Message)"
    }
    Write-Host "Installed know $Version to $(Join-Path $InstallDir 'know.exe')"

    if ($env:KNOWELL_NO_PATH -ne '1') {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $entries = @()
        if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ } }
        $already = $entries | Where-Object { $_.TrimEnd('\') -ieq $InstallDir.TrimEnd('\') }
        if (-not $already) {
            $newPath = (@($entries) + $InstallDir) -join ';'
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Host "Added $InstallDir to your user PATH. Open a new terminal to use 'know'."
        }
    }
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
