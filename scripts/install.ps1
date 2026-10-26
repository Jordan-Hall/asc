<#
.SYNOPSIS
  asc installer for Windows (x86_64). Never interactive; writes only below the install prefix and,
  unless -NoModifyPath is given, the user PATH.

.DESCRIPTION
  $env:ASC_DOWNLOAD_BASE_URL = 'https://downloads.example.org/asc'; ./install.ps1

  The base URL is the directory a release host serves (scripts/release/publish-to-hub.sh writes
  it): <base>/latest/version, <base>/<version>/SHA256SUMS and <base>/<version>/asc-<version>-x86_64-pc-windows-msvc.zip.
  There is no default host.

  Layout, which `asc self-update` continues: <prefix>\<version>\asc.exe and <prefix>\current, a
  junction to the active version. <prefix>\current is what goes on the user PATH.

  Environment: ASC_DOWNLOAD_BASE_URL, ASC_VERSION, ASC_INSTALL_PREFIX (same meaning as the
  parameters), ASC_RELEASE_PUBKEY (minisign public key text: SHA256SUMS must then carry a valid
  signature and minisign must be installed, or the install stops), ASC_ALLOW_LOOPBACK_HTTP=1 (allow
  http:// for 127.0.0.1, localhost and [::1]).

.PARAMETER BaseUrl
  Where releases are served. https; loopback http only with ASC_ALLOW_LOOPBACK_HTTP=1.
.PARAMETER Version
  Install this MAJOR.MINOR.PATCH instead of latest/version.
.PARAMETER Prefix
  Default: $env:LOCALAPPDATA\Programs\asc (not $env:LOCALAPPDATA\asc, which is the asc data directory)
.PARAMETER DryRun
  Resolve and print the plan; download no archive and write nothing.
.PARAMETER NoModifyPath
  Do not add <prefix>\current to the user PATH.
#>
[CmdletBinding()]
param(
  [string]$BaseUrl = $env:ASC_DOWNLOAD_BASE_URL,
  [string]$Version = $env:ASC_VERSION,
  [string]$Prefix = $env:ASC_INSTALL_PREFIX,
  [switch]$DryRun,
  [switch]$NoModifyPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Windows PowerShell 5.1 needs the HTTP assembly loaded and TLS 1.2 switched on; PowerShell 7 has both.
Add-Type -AssemblyName System.Net.Http
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

function Fail([string]$Message) { throw "install.ps1: $Message" }

# ---- the base URL ------------------------------------------------------------------------------

if ([string]::IsNullOrWhiteSpace($BaseUrl)) {
  Fail 'no download location: set ASC_DOWNLOAD_BASE_URL or pass -BaseUrl'
}
$BaseUrl = $BaseUrl.Trim().TrimEnd('/')
$uri = $null
if (-not [Uri]::TryCreate($BaseUrl, [UriKind]::Absolute, [ref]$uri)) { Fail 'the base URL is not a URL' }
if ($uri.UserInfo) { Fail 'the base URL must not contain credentials' }
if ($uri.Query -or $uri.Fragment) { Fail 'the base URL must not have a query or fragment' }
if ($BaseUrl -notmatch '^[A-Za-z0-9._~:/%+,=\[\]@-]+$') { Fail 'the base URL has characters that are not allowed' }
if ($uri.Scheme -eq 'https') {
  # fine
} elseif ($uri.Scheme -eq 'http') {
  $loopback = $uri.IsLoopback
  if (-not $loopback) { Fail 'plain http is refused; use an https base URL' }
  if ($env:ASC_ALLOW_LOOPBACK_HTTP -ne '1') {
    Fail 'plain http is refused; use https (loopback http needs ASC_ALLOW_LOOPBACK_HTTP=1)'
  }
} else {
  Fail 'the base URL must start with https://'
}

# ---- platform ----------------------------------------------------------------------------------

if (-not [Environment]::Is64BitOperatingSystem) { Fail 'asc needs a 64-bit Windows' }
$arch = $env:PROCESSOR_ARCHITEW6432
if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
switch ($arch) {
  'AMD64' { }
  'ARM64' { Write-Warning 'installing the x86_64 build; it runs under Windows emulation on ARM64' }
  default { Fail "no asc build for the processor '$arch' (build: x86_64)" }
}
$target = 'x86_64-pc-windows-msvc'

if (-not $Prefix) {
  if (-not $env:LOCALAPPDATA) { Fail 'LOCALAPPDATA is not set; pass -Prefix' }
  $Prefix = Join-Path (Join-Path $env:LOCALAPPDATA 'Programs') 'asc'
}
if ($Prefix -match '[%"^&]') { Fail 'the install prefix has characters that cannot be used for a junction' }

# ---- downloads ---------------------------------------------------------------------------------

function Get-Bytes([string]$Url, [long]$Max) {
  # Redirects are followed at most 5 times and never leave the scheme of the base URL.
  $handler = [System.Net.Http.HttpClientHandler]::new()
  $handler.MaxAutomaticRedirections = 5
  $client = [System.Net.Http.HttpClient]::new($handler)
  $client.Timeout = [TimeSpan]::FromMinutes(10)
  try {
    $resp = $client.GetAsync($Url, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
    if (-not $resp.IsSuccessStatusCode) { Fail "cannot read $Url ($([int]$resp.StatusCode))" }
    if ($resp.RequestMessage.RequestUri.Scheme -ne $uri.Scheme) { Fail "$Url redirected to another scheme" }
    $len = $resp.Content.Headers.ContentLength
    if ($len -and $len -gt $Max) { Fail "$Url is larger than $Max bytes" }
    $stream = $resp.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
    $ms = [System.IO.MemoryStream]::new()
    $buf = New-Object byte[] 65536
    while (($n = $stream.Read($buf, 0, $buf.Length)) -gt 0) {
      $ms.Write($buf, 0, $n)
      if ($ms.Length -gt $Max) { Fail "$Url is larger than $Max bytes" }
    }
    return , $ms.ToArray()
  } finally {
    $client.Dispose()
  }
}

function Get-Text([string]$Url, [long]$Max) {
  [System.Text.Encoding]::UTF8.GetString((Get-Bytes $Url $Max))
}

function Test-Version([string]$Text) {
  $Text -match '^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$'
}

if (-not $Version) { $Version = (Get-Text "$BaseUrl/latest/version" 64).Trim() }
if (-not (Test-Version $Version)) { Fail "not a version: '$Version' (expected MAJOR.MINOR.PATCH)" }

$archive = "asc-$Version-$target.zip"
$rel = "$BaseUrl/$Version"
$sums = Get-Text "$rel/SHA256SUMS" 1MB

$pubkey = $env:ASC_RELEASE_PUBKEY
if ($pubkey) {
  $minisign = Get-Command minisign -ErrorAction SilentlyContinue
  if (-not $minisign) { Fail 'ASC_RELEASE_PUBKEY is set but minisign is not installed; refusing to continue' }
  $tmpDir = Join-Path ([IO.Path]::GetTempPath()) ("asc-install-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmpDir | Out-Null
  try {
    $sumsFile = Join-Path $tmpDir 'SHA256SUMS'
    $sigFile = Join-Path $tmpDir 'SHA256SUMS.minisig'
    [IO.File]::WriteAllText($sumsFile, $sums, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllBytes($sigFile, (Get-Bytes "$rel/SHA256SUMS.minisig" 4096))
    & $minisign.Source -V -q -P $pubkey -x $sigFile -m $sumsFile *> $null
    if ($LASTEXITCODE -ne 0) { Fail 'the signature of SHA256SUMS is not valid for the configured key; nothing was installed' }
    $comment = (Get-Content -LiteralPath $sigFile | Where-Object { $_ -like 'trusted comment: *' } | Select-Object -First 1)
    $tokens = ($comment -replace '^trusted comment: ', '') -split '[\t ]+'
    if ($tokens -notcontains 'file:SHA256SUMS') { Fail 'the signature is for another file; nothing was installed' }
    if ($tokens -notcontains "version:$Version") { Fail 'the signature is for another version; nothing was installed' }
  } finally {
    Remove-Item -Recurse -Force -LiteralPath $tmpDir -ErrorAction SilentlyContinue
  }
  $trust = 'signed by the configured key'
} else {
  $trust = "checksum only (set ASC_RELEASE_PUBKEY to verify the publisher's signature)"
}

$entries = @($sums -split "`r?`n" | ForEach-Object {
    if ($_ -match '^([0-9a-f]{64})  (\S+)$' -and $Matches[2] -ceq $archive) { $Matches[1] }
  })
if ($entries.Count -ne 1) { Fail "SHA256SUMS has no single entry for $archive" }
$expected = $entries[0]

$dest = Join-Path $Prefix $Version
$current = Join-Path $Prefix 'current'
Write-Host "asc $Version for $target"
Write-Host "  from     $rel/$archive"
Write-Host "  sha256   $expected"
Write-Host "  trust    $trust"
Write-Host "  into     $(Join-Path $dest 'asc.exe')"
Write-Host "  current  $current"

if ($DryRun) {
  Write-Host 'dry run: nothing was downloaded or written'
  return
}

if (-not $pubkey) {
  Write-Warning 'the checksum comes from the same host as the archive; it does not prove who published it'
}

$bytes = Get-Bytes "$rel/$archive" 256MB
$sha = [System.Security.Cryptography.SHA256]::Create()
$got = ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').ToLowerInvariant()
if ($got -ne $expected) { Fail "checksum mismatch for $archive (expected $expected, got $got); nothing was installed" }

# Only a flat asc.exe is read from the archive; no path in it is trusted.
Add-Type -AssemblyName System.IO.Compression
$zip = [System.IO.Compression.ZipArchive]::new([IO.MemoryStream]::new($bytes), [System.IO.Compression.ZipArchiveMode]::Read)
try {
  $exeEntry = @($zip.Entries | Where-Object { $_.FullName -ceq 'asc.exe' })
  if ($exeEntry.Count -ne 1) { Fail 'the archive has no asc.exe' }
  if ($exeEntry[0].Length -le 0 -or $exeEntry[0].Length -gt 512MB) { Fail 'the archive''s asc.exe has an unreasonable size' }
  $exeBytes = New-Object byte[] $exeEntry[0].Length
  $es = $exeEntry[0].Open()
  try {
    $read = 0
    while ($read -lt $exeBytes.Length) {
      $n = $es.Read($exeBytes, $read, $exeBytes.Length - $read)
      if ($n -le 0) { Fail 'the archive''s asc.exe is shorter than it declares' }
      $read += $n
    }
  } finally { $es.Dispose() }
} finally { $zip.Dispose() }

New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
if (Test-Path -LiteralPath $dest) {
  $existing = Join-Path $dest 'asc.exe'
  $same = $false
  if (Test-Path -LiteralPath $existing -PathType Leaf) {
    $have = [BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($existing)))
    $want = [BitConverter]::ToString($sha.ComputeHash($exeBytes))
    $same = ($have -eq $want)
  }
  if (-not $same) { Fail "$dest exists with different content; remove it first" }
  Write-Host "asc $Version was already installed"
} else {
  $stage = Join-Path $Prefix (".$Version.new-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $stage | Out-Null
  try {
    # The directory `asc self-update` expects beside the executable.
    New-Item -ItemType Directory -Path (Join-Path $stage 'shim') | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $stage 'asc.exe'), $exeBytes)
    Move-Item -LiteralPath $stage -Destination $dest
  } catch {
    Remove-Item -Recurse -Force -LiteralPath $stage -ErrorAction SilentlyContinue
    throw
  }
}

# Switch `current`: a junction, as `asc self-update` makes it.
if (Test-Path -LiteralPath $current) {
  $item = Get-Item -LiteralPath $current -Force
  if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    Fail "$current exists and is not a junction; remove it first"
  }
  # Removing a junction leaves its target alone.
  [IO.Directory]::Delete($current)
}
New-Item -ItemType Junction -Path $current -Target $dest | Out-Null

Write-Host "installed asc $Version in $dest"

if (-not $NoModifyPath) {
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $parts = @(); if ($userPath) { $parts = $userPath -split ';' | Where-Object { $_ } }
  if (-not ($parts | Where-Object { $_.TrimEnd('\') -ieq $current.TrimEnd('\') })) {
    [Environment]::SetEnvironmentVariable('Path', (($parts + $current) -join ';'), 'User')
    Write-Host "added $current to your user PATH; open a new terminal to use asc"
  }
} else {
  Write-Host "add $current to your PATH to run asc"
}
