$ErrorActionPreference = 'SilentlyContinue'
$root = Get-Process -Name 'System Monitor Widget-Tauri-Portable-1.0.0' | Select-Object -First 1
if (-not $root) { Write-Output 'NO PROCESS'; exit 1 }

function Info($proc, $label) {
  [pscustomobject]@{
    Role    = $label
    PID     = $proc.Id
    WS_MB   = [math]::Round($proc.WorkingSet64/1MB, 1)
    Priv_MB = [math]::Round($proc.PrivateMemorySize64/1MB, 1)
  }
}

$rows = @()
$rows += Info $root 'host (Rust)'

$cimAll = Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'"
$browsers = $cimAll | Where-Object { $_.ParentProcessId -eq $root.Id }
foreach ($b in $browsers) {
  $gp = Get-Process -Id $b.ProcessId
  $role = if ($b.CommandLine -match '--type=([^\s]+)') { $Matches[1] } else { 'webview-browser' }
  $rows += Info $gp $role
  $kids = $cimAll | Where-Object { $_.ParentProcessId -eq $b.ProcessId }
  foreach ($k in $kids) {
    $gk = Get-Process -Id $k.ProcessId
    $krole = if ($k.CommandLine -match '--type=([^\s]+)') { $Matches[1] } else { 'webview-browser-child' }
    $rows += Info $gk $krole
  }
}
$rows | Format-Table -AutoSize
$ws = ($rows | Measure-Object WS_MB -Sum).Sum
$pv = ($rows | Measure-Object Priv_MB -Sum).Sum
Write-Output ("TOTAL WS: {0} MB   TOTAL PRIVATE: {1} MB" -f [math]::Round($ws,1), [math]::Round($pv,1))
