<#
.SYNOPSIS
  Measure Lightify's startup time, memory, threads and idle CPU (UI-PLAN.md, C6).

.DESCRIPTION
  For each run: launch, wait for the main window, let it settle, sample CPU over a
  window, read memory/threads, then close it with CloseMainWindow (never a kill, so the
  engine retires its Connect device cleanly). Refuses to run while Lightify is already
  open — two instances skew every number.

  -Minimize also measures the window minimized (C2/C3): working set after the trim and
  idle CPU while hidden.

  -Compare Spotify measures the Spotify desktop app the same way (all its processes
  summed). It must not be running either.

.EXAMPLE
  .\measure.ps1                         # 3 runs of target\release\Lightify.exe
  .\measure.ps1 -Runs 5 -Minimize
  .\measure.ps1 -Compare Spotify
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\Lightify.exe'),
    [int]$Runs = 3,
    [int]$SettleSec = 20,
    [int]$SampleSec = 30,
    [switch]$Minimize,
    [ValidateSet('', 'Spotify')][string]$Compare = ''
)
$ErrorActionPreference = 'Stop'

Add-Type -Namespace LightifyMeasure -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
'@

function Get-Sample([string]$name) {
    $ps = @(Get-Process -Name $name -ErrorAction SilentlyContinue)
    [pscustomobject]@{
        Procs   = $ps.Count
        CpuMs   = (($ps | ForEach-Object { $_.TotalProcessorTime.TotalMilliseconds }) | Measure-Object -Sum).Sum
        WsMB    = [math]::Round((($ps | Measure-Object WorkingSet64 -Sum).Sum) / 1MB, 1)
        PrivMB  = [math]::Round((($ps | Measure-Object PrivateMemorySize64 -Sum).Sum) / 1MB, 1)
        Threads = (($ps | ForEach-Object { $_.Threads.Count }) | Measure-Object -Sum).Sum
    }
}

function Measure-App([string]$path, [string]$procName) {
    if (Get-Process -Name $procName -ErrorAction SilentlyContinue) {
        throw "$procName is already running - close it first (two instances skew the numbers)."
    }
    $results = @()
    for ($i = 1; $i -le $Runs; $i++) {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $p = Start-Process -FilePath $path -PassThru
        while ($p.MainWindowHandle -eq 0 -and $sw.Elapsed.TotalSeconds -lt 30) {
            Start-Sleep -Milliseconds 5; $p.Refresh()
        }
        $startMs = [math]::Round($sw.Elapsed.TotalMilliseconds)
        Start-Sleep -Seconds $SettleSec
        $a = Get-Sample $procName
        Start-Sleep -Seconds $SampleSec
        $b = Get-Sample $procName
        $row = [ordered]@{
            Run = $i; StartMs = $startMs; Procs = $b.Procs; WsMB = $b.WsMB; PrivMB = $b.PrivMB
            Threads = $b.Threads; IdleCpuMs = [math]::Round($b.CpuMs - $a.CpuMs)
        }
        if ($Minimize) {
            $p.Refresh()
            [LightifyMeasure.Win]::ShowWindow($p.MainWindowHandle, 6) | Out-Null   # SW_MINIMIZE
            Start-Sleep -Seconds 5
            $c = Get-Sample $procName
            Start-Sleep -Seconds $SampleSec
            $d = Get-Sample $procName
            $row.MinWsMB = $d.WsMB
            $row.MinIdleCpuMs = [math]::Round($d.CpuMs - $c.CpuMs)
            $sw2 = [Diagnostics.Stopwatch]::StartNew()
            [LightifyMeasure.Win]::ShowWindow($p.MainWindowHandle, 9) | Out-Null   # SW_RESTORE
            $row.RestoreMs = [math]::Round($sw2.Elapsed.TotalMilliseconds)
            Start-Sleep -Seconds 2
        }
        $p.Refresh()
        $sw3 = [Diagnostics.Stopwatch]::StartNew()
        $p.CloseMainWindow() | Out-Null
        # Wait for *every* process of the app to be gone (Spotify has helpers), so the
        # next run can't start beside a still-exiting one — Lightify is single-instance
        # and would hand the launch to the old process.
        while ((Get-Process -Name $procName -ErrorAction SilentlyContinue) -and $sw3.Elapsed.TotalSeconds -lt 60) {
            Start-Sleep -Milliseconds 50
        }
        $row.ExitMs = [math]::Round($sw3.Elapsed.TotalMilliseconds)
        if (Get-Process -Name $procName -ErrorAction SilentlyContinue) {
            throw "run ${i}: $procName still running 60 s after CloseMainWindow"
        }
        if ($b.Procs -eq 0) { $row.Note = 'exited during run' }
        $results += [pscustomobject]$row
        Start-Sleep -Seconds 2
    }
    $results
}

$rows = Measure-App (Resolve-Path $Exe).Path 'Lightify'
"`nLightify ($([IO.Path]::GetFileName($Exe)), $([math]::Round((Get-Item $Exe).Length / 1MB, 1)) MB)"
$rows | Format-Table -AutoSize | Out-String

if ($Compare -eq 'Spotify') {
    $spotify = Join-Path $env:APPDATA 'Spotify\Spotify.exe'
    if (-not (Test-Path $spotify)) { throw "Spotify not found at $spotify" }
    $srows = Measure-App $spotify 'Spotify'
    "`nSpotify"
    $srows | Format-Table -AutoSize | Out-String
}
