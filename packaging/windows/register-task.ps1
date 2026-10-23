# asc MSI helper (SPEC 16.2, 11.4). Runs elevated from deferred custom actions; never interactive.
#   -Action Junction   point <Prefix>\current at <Prefix>\<Version>
#   -Action Task       register the logon task that starts the daemon
#   -Action Shim       put <Prefix>\current\shim on the machine PATH immediately before Git\cmd,
#                      and add the ~/.bashrc block for Git Bash
#   -Action Unshim     undo Shim
#   -Action Unregister remove the logon task and the Shim entries
param(
  [Parameter(Mandatory)][ValidateSet('Junction', 'Task', 'Shim', 'Unshim', 'Unregister')][string]$Action,
  [Parameter(Mandatory)][ValidateScript({ [IO.Path]::IsPathRooted($_) -and $_ -notmatch '["%^&|<>]' })][string]$Prefix,
  [ValidatePattern('^\d{1,9}\.\d{1,9}\.\d{1,9}$')][string]$Version = '0.0.0'
)
$ErrorActionPreference = 'Stop'
$Prefix = $Prefix.TrimEnd('\')
$ShimDir = Join-Path $Prefix 'current\shim'
$TaskName = 'asc-daemon'
$BashBegin = '# >>> asc shim >>>'
$BashEnd = '# <<< asc shim <<<'

function Get-PathEntries {
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SYSTEM\CurrentControlSet\Control\Session Manager\Environment')
  try { $raw = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames') } finally { $key.Dispose() }
  @($raw -split ';' | Where-Object { $_ -ne '' })
}

function Set-PathEntries([string[]]$entries) {
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SYSTEM\CurrentControlSet\Control\Session Manager\Environment', $true)
  try { $key.SetValue('Path', ($entries -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString) } finally { $key.Dispose() }
  # Broadcast WM_SETTINGCHANGE so new processes see the change.
  [Environment]::SetEnvironmentVariable('ASC_PATH_REFRESH', [string](Get-Date).Ticks, 'Machine')
  [Environment]::SetEnvironmentVariable('ASC_PATH_REFRESH', $null, 'Machine')
}

function Remove-Shim {
  $entries = Get-PathEntries | Where-Object { $_.TrimEnd('\') -ne $ShimDir }
  Set-PathEntries $entries
}

function Add-Shim {
  # Any older asc shim entry goes first, so the entry is never duplicated.
  $entries = @(Get-PathEntries | Where-Object { $_.TrimEnd('\') -ne $ShimDir })
  $git = $null
  if ($entries.Count -gt 0) { $git = 0..($entries.Count - 1) | Where-Object { $entries[$_].TrimEnd('\') -like '*\Git\cmd' } | Select-Object -First 1 }
  if ($null -eq $git) { $new = @($ShimDir) + $entries }
  else {
    $before = @()
    if ($git -gt 0) { $before = @($entries[0..($git - 1)]) }
    $new = $before + @($ShimDir) + @($entries[$git..($entries.Count - 1)])
  }
  Set-PathEntries $new
}

function Get-BashProfiles {
  $skip = 'Public', 'All Users', 'Default User', 'defaultuser0'
  Get-ChildItem 'C:\Users' -Directory -ErrorAction SilentlyContinue |
    Where-Object { $skip -notcontains $_.Name -and -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) } |
    ForEach-Object { $_.FullName }
}

function Set-BashBlock([string]$home_, [bool]$on) {
  $rc = Join-Path $home_ '.bashrc'
  $text = if (Test-Path -LiteralPath $rc) { [IO.File]::ReadAllText($rc) } else { '' }
  $text = [regex]::Replace($text, "(?s)\r?\n?$([regex]::Escape($BashBegin)).*?$([regex]::Escape($BashEnd))\r?\n?", "`n")
  if ($on) {
    $drive = $Prefix.Substring(0, 1).ToLower()
    $unix = "/$drive" + ($Prefix.Substring(2) -replace '\', '/')
    $text = $text.TrimEnd() + "`n$BashBegin`nexport PATH=`"$unix/current/shim:`$PATH`"`n$BashEnd`n"
  }
  [IO.File]::WriteAllText($rc, $text, (New-Object Text.UTF8Encoding($false)))
}

switch ($Action) {
  'Junction' {
    $cur = Join-Path $Prefix 'current'
    $target = Join-Path $Prefix $Version
    if (-not (Test-Path -LiteralPath $target -PathType Container)) { throw "missing $target" }
    if (Test-Path -LiteralPath $cur) {
      $item = Get-Item -LiteralPath $cur -Force
      if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "$cur exists and is not a junction" }
      [IO.Directory]::Delete($cur)   # removes the junction, never its target
    }
    New-Item -ItemType Junction -Path $cur -Target $target | Out-Null
  }
  'Task' {
    $exe = Join-Path $Prefix 'current\asc.exe'
    $act = New-ScheduledTaskAction -Execute $exe -Argument 'daemon start'
    $trg = New-ScheduledTaskTrigger -AtLogOn
    $pri = New-ScheduledTaskPrincipal -GroupId 'S-1-5-32-545' -RunLevel Limited
    $set = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5) -Hidden
    Register-ScheduledTask -TaskName $TaskName -Action $act -Trigger $trg -Principal $pri -Settings $set -Force | Out-Null
  }
  'Shim' {
    Add-Shim
    foreach ($p in Get-BashProfiles) { Set-BashBlock $p $true }
  }
  'Unshim' {
    Remove-Shim
    foreach ($p in Get-BashProfiles) { Set-BashBlock $p $false }
  }
  'Unregister' {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Shim
    foreach ($p in Get-BashProfiles) { Set-BashBlock $p $false }
  }
}
