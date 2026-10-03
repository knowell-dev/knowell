<#
.SYNOPSIS
  Bootstrap a new private direct Knowell installation without administrator rights.
.DESCRIPTION
  Downloads digest-prefixed raw runtime and launcher components. Existing destinations
  are refused: installed copies use know update. The initial checksum/provenance trust
  boundary is separate from native TUF authorization; see docs/RELEASING.md.
#>
[CmdletBinding()]
param(
    [string]$Version = $env:KNOWELL_VERSION,
    [string]$InstallDir = $env:KNOWELL_INSTALL_DIR,
    [ValidateSet('auto', 'require', 'skip')]
    [string]$Attestation = 'auto'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }
$ProgressPreference = 'SilentlyContinue'
$maxBinary = 536870912
$tmp = $null
$lock = $null
$Repo = if ($env:KNOWELL_REPO) { $env:KNOWELL_REPO } else { 'knowell-dev/knowell' }
function Fail([string]$Message) { throw "knowell install: $Message" }
if ($Repo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or $Repo.Contains('..')) { Fail 'invalid repository identity' }

function Test-UnlinkedPath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { Fail 'installation path must not contain reparse points' }
        }
        $next = [IO.Path]::GetDirectoryName($cursor)
        if ($next -eq $cursor) { break }
        $cursor = $next
    }
}
function Protect-Directory([string]$Path) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner($identity.User)
    $inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    foreach ($sid in @($identity.User, (New-Object Security.Principal.SecurityIdentifier('S-1-5-18')), (New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')))) {
        $rule = New-Object Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', $inherit, 'None', 'Allow')
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}
function Get-LatestVersion {
    try {
        $response = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" -Method Head -UseBasicParsing -TimeoutSec 60
        $final = if ($response.BaseResponse.PSObject.Properties['ResponseUri']) { $response.BaseResponse.ResponseUri.AbsoluteUri } else { $response.BaseResponse.RequestMessage.RequestUri.AbsoluteUri }
        $tag = ($final -split '/')[-1]
        if ($tag -notmatch '^v[0-9]') { Fail 'could not determine the latest release; use -Version' }
        return $tag.Substring(1)
    } catch { Fail 'could not determine the latest release; use -Version' }
}
function Test-DownloadUri([Uri]$Uri) {
    if ($Uri.UserInfo -or $Uri.Fragment) { Fail 'download URLs must not contain credentials or fragments' }
    if ($Uri.Scheme -eq 'https' -or $Uri.Scheme -eq 'file') { return }
    if ($Uri.Scheme -eq 'http' -and $Uri.IsLoopback) { return }
    Fail 'download URLs must use HTTPS'
}
function Get-BoundedFile([string]$Url, [string]$Destination, [long]$Maximum) {
    $uri = New-Object Uri($Url)
    Test-DownloadUri $uri
    $inputStream = $null
    $outputStream = $null
    $response = $null
    try {
        if ($uri.Scheme -eq 'file') {
            $inputStream = [IO.File]::OpenRead($uri.LocalPath)
            if ($inputStream.Length -le 0 -or $inputStream.Length -gt $Maximum) { Fail 'download size is outside its byte limit' }
        } else {
            for ($redirects = 0; $redirects -le 5; $redirects++) {
                $request = [Net.HttpWebRequest]::Create($uri)
                $request.AllowAutoRedirect = $false
                $request.Timeout = 60000
                $request.ReadWriteTimeout = 60000
                $request.UserAgent = 'knowell-bootstrap'
                $request.Headers['Accept-Encoding'] = 'identity'
                $response = $request.GetResponse()
                $status = [int]$response.StatusCode
                if ($status -ge 300 -and $status -lt 400) {
                    $next = New-Object Uri($uri, $response.Headers['Location'])
                    Test-DownloadUri $next
                    $sameOrigin = ($next.Scheme -eq $uri.Scheme -and $next.Authority -eq $uri.Authority)
                    $githubCdn = ($uri.Host -eq 'github.com' -and $next.Scheme -eq 'https' -and $next.Host -in @('release-assets.githubusercontent.com', 'objects.githubusercontent.com'))
                    if ($redirects -eq 5 -or (-not $sameOrigin -and -not $githubCdn)) { Fail 'refusing a download redirect' }
                    $response.Dispose()
                    $response = $null
                    $uri = $next
                    continue
                }
                if ($status -ne 200 -or $response.ContentLength -gt $Maximum) { Fail 'download failed or exceeded its byte limit' }
                $encoding = $response.Headers['Content-Encoding']
                if ($encoding -and $encoding -ne 'identity') { Fail 'unsupported download encoding' }
                $inputStream = $response.GetResponseStream()
                break
            }
        }
        if (-not $inputStream) { Fail 'download did not return a stream' }
        $outputStream = [IO.File]::Open($Destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $buffer = New-Object byte[] 65536
        $received = [long]0
        $deadline = [DateTime]::UtcNow.AddSeconds(300)
        while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $received += $read
            if ($received -gt $Maximum -or [DateTime]::UtcNow -gt $deadline) { Fail 'download exceeded its byte or time limit' }
            $outputStream.Write($buffer, 0, $read)
        }
        if ($received -eq 0 -or ($response -and $response.ContentLength -ge 0 -and $received -ne $response.ContentLength)) { Fail 'download is empty or truncated' }
        $outputStream.Flush($true)
    } catch { Fail 'download failed, was truncated or exceeded its byte limit' }
    finally {
        if ($outputStream) { $outputStream.Dispose() }
        if ($inputStream) { $inputStream.Dispose() }
        if ($response) { $response.Dispose() }
    }
}
function Get-RawAsset([string]$Sums, [string]$Basename) {
    $found = @()
    foreach ($line in [IO.File]::ReadAllLines($Sums)) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -ne 2 -or $parts[0] -notmatch '^[0-9a-fA-F]{64}$') { continue }
        $digest = $parts[0].ToLowerInvariant()
        $name = $parts[1].TrimStart('*')
        if ($name -ceq "$digest.$Basename") { $found += @{ Name = $name; Digest = $digest } }
    }
    if ($found.Count -ne 1) { Fail 'SHA256SUMS must list exactly one canonical raw component' }
    return $found[0]
}
function Write-Json([string]$Path, [object]$Value) {
    $utf8 = New-Object Text.UTF8Encoding($false)
    [IO.File]::WriteAllText($Path, (($Value | ConvertTo-Json -Compress) + "`n"), $utf8)
}
if (-not $Version) { Write-Host 'Looking up the latest Knowell release ...'; $Version = Get-LatestVersion }
$Version = $Version -replace '^v', ''
if ($Version.Length -gt 128 -or $Version -notmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$') { Fail 'invalid version' }
$numericVersion = ($Version -split '-', 2)[0] -split '\.'
foreach ($number in $numericVersion) {
    $parsed = [uint64]0
    if (-not [uint64]::TryParse($number, [ref]$parsed)) { Fail 'invalid version' }
}
$preIndex = $Version.IndexOf('-')
if ($preIndex -ge 0) {
    foreach ($identifier in ($Version.Substring($preIndex + 1) -split '\.')) {
        if ($identifier -match '^0[0-9]+$') { Fail 'invalid version' }
    }
}
if ($Version -match '^0\.' -or $Version -match '^1\.0\.0-') { Fail 'public installations require the first stable 1.0 release or later' }
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ($arch) {
    'AMD64' { $target = 'x86_64-pc-windows-msvc' }
    'ARM64' { $target = 'aarch64-pc-windows-msvc' }
    default { Fail 'unsupported CPU architecture' }
}
if (-not $InstallDir) {
    if (-not $env:LOCALAPPDATA) { Fail 'LOCALAPPDATA is not set; pass -InstallDir' }
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\knowell'
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
Test-UnlinkedPath $InstallDir
if (Test-Path -LiteralPath $InstallDir) { Fail 'installation destination already exists; use know update or an explicit repair procedure' }
$parent = [IO.Path]::GetDirectoryName($InstallDir)
New-Item -ItemType Directory -Path $parent -Force | Out-Null
try {
    $candidateLock = "$InstallDir.bootstrap-lock"
    New-Item -ItemType Directory -Path $candidateLock | Out-Null
    $lock = $candidateLock
    Protect-Directory $lock
    if (Test-Path -LiteralPath $InstallDir) { Fail 'installation destination changed during bootstrap' }
    $tmp = Join-Path $parent ('.knowell-bootstrap-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    Protect-Directory $tmp
    $base = if ($env:KNOWELL_DOWNLOAD_BASE) { $env:KNOWELL_DOWNLOAD_BASE } else { "https://github.com/$Repo/releases/download" }
    $base = $base.TrimEnd('/') + "/v$Version"
    $sums = Join-Path $tmp 'SHA256SUMS'
    Get-BoundedFile "$base/SHA256SUMS" $sums 1048576
    $engine = Get-RawAsset $sums "knowell-$Version-$target-engine.exe"
    $launcher = Get-RawAsset $sums "knowell-$Version-$target-launcher.exe"
    $enginePath = Join-Path $tmp 'engine.exe'
    $launcherPath = Join-Path $tmp 'launcher.exe'
    Write-Host "Downloading Knowell $Version for $target ..."
    Get-BoundedFile "$base/$($engine.Name)" $enginePath $maxBinary
    Get-BoundedFile "$base/$($launcher.Name)" $launcherPath $maxBinary
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $enginePath).Hash.ToLowerInvariant() -ne $engine.Digest) { Fail 'runtime checksum mismatch; nothing was installed' }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $launcherPath).Hash.ToLowerInvariant() -ne $launcher.Digest) { Fail 'launcher checksum mismatch; nothing was installed' }
    Write-Host 'Checksum verified. Initial bootstrap trust is independent of native TUF update verification.'
    if ($Attestation -ne 'skip') {
        $gh = Get-Command gh -ErrorAction SilentlyContinue
        $loggedIn = $false
        if ($gh) { & gh auth status *> $null; $loggedIn = ($LASTEXITCODE -eq 0) }
        if ($gh -and $loggedIn) {
            foreach ($component in @($enginePath, $launcherPath)) {
                & gh attestation verify $component --repo $Repo --signer-workflow "$Repo/.github/workflows/release.yml" --source-ref "refs/tags/v$Version" *> $null
                if ($LASTEXITCODE -ne 0) { Fail 'attestation verification failed; nothing was installed' }
            }
            Write-Host 'Build provenance attestation verified.'
        } elseif ($Attestation -eq 'require') { Fail '-Attestation require needs gh, logged in' }
        else { Write-Host 'Skipping attestation check (use -Attestation require to require provenance).' }
    }
    $stage = Join-Path $tmp 'install'
    $runtimeDir = Join-Path $stage "versions\$Version\$target"
    New-Item -ItemType Directory -Path $runtimeDir -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $stage 'metadata') | Out-Null
    Protect-Directory $stage
    Move-Item -LiteralPath $enginePath -Destination (Join-Path $runtimeDir 'know.exe')
    Move-Item -LiteralPath $launcherPath -Destination (Join-Path $stage 'know.exe')
    Write-Json (Join-Path $stage 'install.json') ([ordered]@{ format_version = 1; owner = 'direct'; target = $target; launcher_protocol = 1 })
    Write-Json (Join-Path $stage 'current.json') ([ordered]@{ format_version = 1; version = $Version; target = $target; sha256 = $engine.Digest; size = (Get-Item -LiteralPath (Join-Path $runtimeDir 'know.exe')).Length })
    Write-Json (Join-Path $stage 'launcher.json') ([ordered]@{ format_version = 1; version = $Version; target = $target; sha256 = $launcher.Digest; size = (Get-Item -LiteralPath (Join-Path $stage 'know.exe')).Length })
    # Directory.Move refuses an existing destination, including a concurrent installer.
    [IO.Directory]::Move($stage, $InstallDir)
    Write-Host "Installed know $Version with a private immutable runtime."
    if ($env:KNOWELL_NO_PATH -ne '1') {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $entries = @()
        if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ } }
        if (-not ($entries | Where-Object { $_.TrimEnd('\') -ieq $InstallDir.TrimEnd('\') })) {
            [Environment]::SetEnvironmentVariable('Path', ((@($entries) + $InstallDir) -join ';'), 'User')
            Write-Host 'Added the dedicated installation directory to user PATH; open a new terminal.'
        }
    }
    Write-Host 'Native updates require an operator-provisioned trusted TUF repository.'
} finally {
    # Only the fresh, canonical staging directory and lock are removed; never the install root.
    if ($tmp) {
        $resolvedTemp = [IO.Path]::GetFullPath($tmp)
        $prefix = [IO.Path]::GetFullPath($parent).TrimEnd('\') + '\.knowell-bootstrap-'
        if ($resolvedTemp.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { Remove-Item -LiteralPath $resolvedTemp -Recurse -Force -ErrorAction SilentlyContinue }
    }
    if ($lock) { Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue }
}
