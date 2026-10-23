# asc MSI helper: Defender exclusions for $ASC_HOME (the object cache and stores) and the asc
# processes (SPEC 16.2). Elevated, non-interactive; a machine without Defender is fine.
param(
  [Parameter(Mandatory)][ValidateSet('Add', 'Remove')][string]$Action,
  [Parameter(Mandatory)][ValidateScript({ [IO.Path]::IsPathRooted($_) -and $_ -notmatch '["%^&|<>]' })][string]$Prefix
)
$ErrorActionPreference = 'Stop'
if (-not (Get-Command Add-MpPreference -ErrorAction SilentlyContinue)) { exit 0 }
$paths = @()
foreach ($u in Get-ChildItem 'C:\Users' -Directory -ErrorAction SilentlyContinue) {
  if ($u.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
  $paths += Join-Path $u.FullName 'AppData\Local\asc'   # the default $ASC_HOME
}
$proc = Join-Path $Prefix 'current\asc.exe'
try {
  if ($Action -eq 'Add') {
    Add-MpPreference -ExclusionPath $paths -ExclusionProcess $proc
  } else {
    Remove-MpPreference -ExclusionPath $paths -ExclusionProcess $proc
  }
} catch {
  # Tamper protection or a managed policy can refuse; installing must not fail on it.
  Write-Warning "Defender exclusions were not changed: $_"
}
