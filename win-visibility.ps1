param([string]$ProcName = 'System Monitor Widget-Tauri-Portable-1.0.0')
$p = Get-Process -Name $ProcName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Output 'NO_PROCESS'; exit }
$targetPid = $p.Id
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WinVis {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lp);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr hWnd, StringBuilder sb, int max);
  public static string Probe(uint target) {
    bool found = false; bool visible = false;
    EnumWindows((h, lp) => {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid == target) {
        var c = new StringBuilder(256); GetClassName(h, c, 256);
        if (c.ToString() == "Tauri Window") { found = true; visible = IsWindowVisible(h); }
      }
      return true;
    }, IntPtr.Zero);
    if (!found) return "MAIN_WINDOW_NOT_FOUND";
    return "MAIN_VISIBLE=" + (visible ? "True" : "False");
  }
}
'@
Write-Output ([WinVis]::Probe($targetPid))
