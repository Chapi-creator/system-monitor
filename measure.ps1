param([string]$Label)
$w = Get-Process -Name system-monitor-widget -ErrorAction Stop
$pw = $w.TotalProcessorTime.TotalSeconds
$pt = -1.0
$prev = [DateTime]::Now
$wv = @(); $tv = @(); $tpHits = 0
for ($i = 0; $i -lt 60; $i++) {
  Start-Sleep -Milliseconds 1000
  $now = [DateTime]::Now
  $w.Refresh()
  $c = $w.TotalProcessorTime.TotalSeconds
  $wv += ($c - $pw) / ($now - $prev).TotalSeconds * 100
  $pw = $c
  $tp = Get-Process -Name typeperf -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($tp) {
    $tpHits++
    $c2 = $tp.TotalProcessorTime.TotalSeconds
    if ($pt -ge 0) { $tv += ($c2 - $pt) / ($now - $prev).TotalSeconds * 100 }
    $pt = $c2
  } else { $tv += $null }
  $prev = $now
}
$wa = ($wv | Measure-Object -Average).Average
$wm = ($wv | Measure-Object -Maximum).Maximum
$pk = ($wv | Where-Object { $_ -gt 2 }).Count
$tvn = @($tv | Where-Object { $null -ne $_ })
$ta = if ($tvn.Count -gt 0) { ($tvn | Measure-Object -Average).Average } else { -1 }
$ram = [math]::Round($w.WorkingSet64 / 1MB, 1)
$tpTxt = if ($ta -ge 0) { "{0:N2}%" -f $ta } else { "absent" }
Write-Output ("{0}: widget avg={1:N2}% max={2:N1}% spikes>2%={3} RAM={4}MB | typeperf present={5}/60 avg={6}" -f $Label, $wa, $wm, $pk, $ram, $tpHits, $tpTxt)
