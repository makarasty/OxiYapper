<#
.SYNOPSIS
  PNG of the real settings window (runtime renderer) without taking focus from the owner.
.EXAMPLE
  scripts/real-shot.ps1 -Page home -Width 1000 -Height 700 -Theme dark -Out C:/t/shots/x/a.png
.NOTES
  Starts the `real_window` example (built first unless -NoBuild), which places the window off every
  screen, saves the runtime renderer's own snapshot of it to -Out once the window has settled and
  exits (no OS capture: PrintWindow of an off-screen window came back flat white in 4 runs of 10).
  Only the process started here is closed. -Tab is passed on for pages that get sub-tabs. Fails
  (non-zero) when the runtime renderer is not skia-software (-AllowSoftware accepts the software
  one) or when the shot window took the foreground. The Audio page opens the default microphone
  for its meter (in memory).
#>
param(
  [string]$Page = 'home',
  [int]$Width = 1000,
  [int]$Height = 700,
  [ValidateSet('light', 'dark')][string]$Theme = 'light',
  [string]$Tab = '',
  [Parameter(Mandatory)][string]$Out,
  [switch]$NoBuild,
  [switch]$AllowSoftware,
  [int]$TimeoutSec = 30
)
$ErrorActionPreference = 'Stop'
$Out = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Out)
$tmp = [System.IO.Path]::GetTempPath()  # the same call Rust's env::temp_dir makes
$root = Split-Path -Parent $PSScriptRoot
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = 'C:/t/oxi' }
if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:/Program Files/LLVM/bin' }

if (-not $NoBuild) {
  Push-Location $root
  try { cargo build -q -p oxi-settings --example real_window; if ($LASTEXITCODE) { throw 'cargo build failed' } }
  finally { Pop-Location }
}
$exe = Join-Path $env:CARGO_TARGET_DIR 'debug/examples/real_window.exe'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Shot {
  delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr p);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [StructLayout(LayoutKind.Sequential)] struct RECT { public int L, T, R, B; }
  public static IntPtr FindWindow(uint pid) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, p) => {
      uint owner; GetWindowThreadProcessId(h, out owner);
      RECT r;
      if (owner == pid && IsWindowVisible(h) && GetClientRect(h, out r) && r.R > 100 && r.B > 100) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
'@

$err = Join-Path $tmp "oxi-real-shot-$PID.err"
$fg = [Shot]::GetForegroundWindow()
$argList = @($Page, $Width, $Height, $Theme, "`"$Out`"") + $(if ($Tab) { @($Tab) } else { @() })
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Out) | Out-Null
Remove-Item -Force $Out -ErrorAction SilentlyContinue
$p = Start-Process -FilePath $exe -ArgumentList $argList -PassThru -RedirectStandardError $err
$null = $p.Handle  # without it ExitCode stays empty once the process has gone
try {
  # The example writes the PNG itself and exits; meanwhile watch that its window never takes the
  # foreground. Only our window taking focus is a failure; the owner switching windows is not.
  $took = $false
  $h = [IntPtr]::Zero
  if (-not $p.WaitForExit(100)) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    do {
      if ($h -eq [IntPtr]::Zero) { $h = [Shot]::FindWindow([uint32]$p.Id) }
      if ($h -ne [IntPtr]::Zero -and [Shot]::GetForegroundWindow() -eq $h) { $took = $true }
    } until ($p.WaitForExit(50) -or (Get-Date) -gt $deadline)
  }
  if (-not $p.HasExited) { throw "no shot in ${TimeoutSec}s" }
  $log = Get-Content $err -ErrorAction SilentlyContinue
  if ($p.ExitCode -ne 0 -or -not (Test-Path $Out)) { throw "real_window failed (exit $($p.ExitCode)): $($log -join ' / ')" }
  $after = [Shot]::GetForegroundWindow()
  $renderer = ($log | Where-Object { $_ -like 'real_window: renderer *' } | Select-Object -First 1)
  $name = "$renderer".Replace('real_window: renderer ', '')
  $size = ("$($log | Where-Object { $_ -like 'real_window: shot *' } | Select-Object -First 1)").Replace('real_window: shot ', '')
  Write-Output "saved $Out ($size) renderer=$name foreground before=$fg after=$after kept=$(-not $took)"
  if ($name -ne 'skia-software' -and -not ($AllowSoftware -and $name -eq 'software')) {
    throw "renderer is '$name', not skia-software (pass -AllowSoftware to accept the software one)"
  }
  if ($took) { throw 'the shot window took the foreground' }
}
finally {
  if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
  $p.WaitForExit()
  Remove-Item -Recurse -Force (Join-Path $tmp "oxi-real-shot-$($p.Id)"), $err -ErrorAction SilentlyContinue
}
