$p = Get-Process -Name system-monitor-widget -ErrorAction Stop
Add-Type -MemberDefinition '[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);' -Name U32 -Namespace W
[W.U32]::PostMessage($p.MainWindowHandle, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
Start-Sleep -Seconds 3
$p.Refresh()
Write-Output ("title=[" + $p.MainWindowTitle + "]")
