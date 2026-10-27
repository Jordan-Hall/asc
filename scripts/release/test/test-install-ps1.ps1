<#
.SYNOPSIS
  Tests scripts/install.ps1 against a local fixture release host (127.0.0.1 only).
  pwsh -NoProfile -File scripts/release/test/test-install-ps1.ps1
  ASC_TEST_PS=powershell.exe runs install.ps1 itself under Windows PowerShell 5.1.
  Needs python (3.11 or newer) on PATH for the release assembler and the fixture server. Never touches the
  real user PATH (-NoModifyPath) or the real LOCALAPPDATA (-Prefix under a temp directory).
#>
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $IsWindows) { Write-Host 'skip: install.ps1 is for Windows'; exit 0 }

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$rel = Join-Path $root 'scripts\release'
$install = Join-Path $root 'scripts\install.ps1'
$py = (Get-Command python -ErrorAction Stop).Source
$work = Join-Path ([IO.Path]::GetTempPath()) ("asc-ps1-test-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
$script:passed = 0
$script:failed = 0
$server = $null

function Ok([string]$Name) { $script:passed++; Write-Host "ok   $Name" }
function Bad([string]$Name, [string]$Detail = '') { $script:failed++; Write-Host "FAIL $Name" -ForegroundColor Red; if ($Detail) { Write-Host "     | $Detail" } }
function Check([string]$Name, [bool]$Cond, [string]$Detail = '') { if ($Cond) { Ok $Name } else { Bad $Name $Detail } }

# Runs install.ps1 in a fresh pwsh and returns @{ Code; Out }.
function Run-Install([string[]]$ScriptArgs, [hashtable]$Env = @{}) {
  # ASC_TEST_PS names the shell that runs install.ps1 (powershell.exe tests Windows PowerShell 5.1).
  $shell = if ($env:ASC_TEST_PS) { $env:ASC_TEST_PS } else { (Get-Process -Id $PID).Path }
  $psi = [Diagnostics.ProcessStartInfo]::new($shell)
  foreach ($a in @('-NoProfile', '-NonInteractive', '-File', $install) + $ScriptArgs) { $psi.ArgumentList.Add($a) }
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  # A Windows PowerShell child must not see PowerShell 7's module path.
  if ($env:ASC_TEST_PS) { $psi.Environment.Remove('PSModulePath') | Out-Null }
  foreach ($k in 'ASC_DOWNLOAD_BASE_URL', 'ASC_VERSION', 'ASC_INSTALL_PREFIX', 'ASC_RELEASE_PUBKEY', 'ASC_ALLOW_LOOPBACK_HTTP') { $psi.Environment.Remove($k) | Out-Null }
  foreach ($k in $Env.Keys) { $psi.Environment[$k] = $Env[$k] }
  $p = [Diagnostics.Process]::Start($psi)
  $out = $p.StandardOutput.ReadToEnd() + $p.StandardError.ReadToEnd()
  $p.WaitForExit()
  return @{ Code = $p.ExitCode; Out = $out }
}

try {
  # ---- a release host --------------------------------------------------------------------------
  $bins = Join-Path $work 'bins'
  foreach ($t in 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-apple-darwin', 'aarch64-apple-darwin') {
    New-Item -ItemType Directory -Path (Join-Path $bins $t) | Out-Null
    Set-Content -NoNewline -Path (Join-Path $bins "$t\asc") -Value "#!/bin/sh`necho asc`n"
  }
  New-Item -ItemType Directory -Path (Join-Path $bins 'x86_64-pc-windows-msvc') | Out-Null
  Set-Content -NoNewline -Path (Join-Path $bins 'x86_64-pc-windows-msvc\asc.exe') -Value 'MZ fake executable 1.2.3'

  $host_ = Join-Path $work 'host'
  New-Item -ItemType Directory -Path "$host_\1.2.3", "$host_\latest" | Out-Null
  & $py -I (Join-Path $rel 'assemble.py') --version 1.2.3 --commit 0123456789abcdef0123456789abcdef01234567 --epoch 1700000000 `
    --bins $bins --cargo-lock (Join-Path $root 'Cargo.lock') `
    --license (Join-Path $root 'LICENSE-APACHE') --license (Join-Path $root 'LICENSE-MIT') `
    --release-url 'https://downloads.example.invalid/asc/1.2.3' --homepage 'https://example.invalid/asc' `
    --root $root --out (Join-Path $work 'release')
  if ($LASTEXITCODE -ne 0) { throw 'assemble.py failed' }
  Copy-Item -Recurse -Force "$work\release\*" "$host_\1.2.3"
  Set-Content -NoNewline -Path "$host_\latest\version" -Value "1.2.3`n"

  $portFile = Join-Path $work 'port'
  $logFile = Join-Path $work 'requests.log'
  $server = Start-Process -PassThru -WindowStyle Hidden -FilePath $py -ArgumentList @('-I', (Join-Path $rel 'test\fixture_server.py'), $host_, $portFile, $logFile)
  for ($i = 0; $i -lt 100 -and -not ((Test-Path $portFile) -and (Get-Item $portFile).Length -gt 0); $i++) { Start-Sleep -Milliseconds 100 }
  $base = "http://127.0.0.1:$(Get-Content $portFile)"
  $loop = @{ ASC_ALLOW_LOOPBACK_HTTP = '1'; ASC_DOWNLOAD_BASE_URL = $base }

  # ---- dry run ---------------------------------------------------------------------------------
  $prefix = Join-Path $work 'prefix'
  $r = Run-Install @('-DryRun', '-Prefix', $prefix, '-NoModifyPath') $loop
  Check 'dry run succeeds' ($r.Code -eq 0) $r.Out
  Check 'dry run names the archive' ($r.Out -like "*$base/1.2.3/asc-1.2.3-x86_64-pc-windows-msvc.zip*") $r.Out
  Check 'dry run writes nothing' (-not (Test-Path $prefix))
  Check 'dry run downloads no archive' (-not ((Get-Content $logFile -Raw) -match '\.zip'))

  # ---- install ---------------------------------------------------------------------------------
  $r = Run-Install @('-Prefix', $prefix, '-NoModifyPath') $loop
  Check 'install succeeds' ($r.Code -eq 0) $r.Out
  Check 'asc.exe is in the version directory' (Test-Path "$prefix\1.2.3\asc.exe" -PathType Leaf)
  Check 'the shim directory exists for self-update' (Test-Path "$prefix\1.2.3\shim" -PathType Container)
  Check 'current is a junction to the version' ((Get-Item "$prefix\current" -Force).LinkType -eq 'Junction' -and (Get-Item "$prefix\current").Target -contains "$prefix\1.2.3")
  Check 'asc.exe is reachable through current' ((Get-Content "$prefix\current\asc.exe" -Raw) -eq 'MZ fake executable 1.2.3')
  Check 'without -NoModifyPath advice is printed' ($r.Out -like '*add*PATH*')
  $r = Run-Install @('-Prefix', $prefix, '-NoModifyPath') $loop
  Check 'installing the same version again is a no-op' ($r.Code -eq 0 -and $r.Out -like '*was already installed*') $r.Out

  # ---- refusals --------------------------------------------------------------------------------
  function Refuses([string]$Name, [string[]]$A, [hashtable]$E, [string]$Needle = '') {
    $r = Run-Install $A $E
    Check $Name ($r.Code -ne 0 -and ($Needle -eq '' -or $r.Out -like "*$Needle*")) $r.Out
    return $r
  }
  $p2 = Join-Path $work 'p2'
  Refuses 'no base URL is refused' @('-Prefix', $p2, '-NoModifyPath') @{} 'ASC_DOWNLOAD_BASE_URL' | Out-Null
  Refuses 'plain http to a remote host is refused' @('-BaseUrl', 'http://releases.example.invalid/asc', '-Prefix', $p2) @{ ASC_ALLOW_LOOPBACK_HTTP = '1' } 'plain http' | Out-Null
  Refuses 'loopback http needs the opt-in' @('-BaseUrl', $base, '-Prefix', $p2) @{} 'ASC_ALLOW_LOOPBACK_HTTP=1' | Out-Null
  $r = Refuses 'credentials in the base URL are refused' @('-BaseUrl', 'https://user:hunter2@releases.example.invalid/asc', '-Prefix', $p2) @{} 'credentials'
  Check 'the credentials are not echoed' (-not ($r.Out -like '*hunter2*')) $r.Out
  Refuses 'a query in the base URL is refused' @('-BaseUrl', 'https://releases.example.invalid/asc?token=abc', '-Prefix', $p2) @{} 'query' | Out-Null
  foreach ($v in '1.2', '01.2.3', '1.2.3-rc1', '..\1.2.3') {
    Refuses "version '$v' is refused" @('-Version', $v, '-Prefix', $p2, '-NoModifyPath') $loop 'not a version' | Out-Null
  }
  Refuses 'an unpublished version is refused' @('-Version', '9.9.9', '-Prefix', $p2, '-NoModifyPath') $loop | Out-Null
  Check 'nothing was installed by the refusals' (-not (Test-Path $p2))

  # A tampered archive stops at the checksum and installs nothing.
  $zip = "$host_\1.2.3\asc-1.2.3-x86_64-pc-windows-msvc.zip"
  $orig = [IO.File]::ReadAllBytes($zip)
  [IO.File]::WriteAllBytes($zip, $orig + [byte[]](1, 2, 3))
  $p3 = Join-Path $work 'p3'
  Refuses 'a tampered archive is refused' @('-Prefix', $p3, '-NoModifyPath') $loop 'checksum mismatch' | Out-Null
  Check 'nothing was installed from the tampered archive' (-not (Test-Path $p3))
  [IO.File]::WriteAllBytes($zip, $orig)

  # A configured key with no minisign on PATH stops the install.
  $noMs = @{ ASC_RELEASE_PUBKEY = 'RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3' } + $loop
  if (-not (Get-Command minisign -ErrorAction SilentlyContinue)) {
    Refuses 'a key without minisign stops the install' @('-Prefix', $p3, '-NoModifyPath') $noMs 'minisign' | Out-Null
    Check 'nothing was installed without minisign' (-not (Test-Path $p3))
  } else {
    Write-Host 'skip the no-minisign case (minisign is installed)'
  }
}
finally {
  if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
  # Junctions first, so the recursive delete cannot follow them.
  Get-ChildItem -Path $work -Recurse -Force -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.LinkType -eq 'Junction' } | ForEach-Object { [IO.Directory]::Delete($_.FullName) }
  Remove-Item -Recurse -Force -LiteralPath $work -ErrorAction SilentlyContinue
}

Write-Host "$($script:passed) passed, $($script:failed) failed"
exit ([int]($script:failed -gt 0))
