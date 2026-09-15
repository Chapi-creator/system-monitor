'use strict';
/**
 * probe-impact.js — Mide el impacto REAL en CPU/RAM del árbol completo de la
 * app (host Rust + procesos WebView2) con las 3 features nuevas activas:
 *   A) Visible en modo Gráficas (6 charts + tarjeta sesión) → CPU 30 s + RAM.
 *   B) Oculto en bandeja (gating de visibilidad) → CPU 20 s + RAM.
 * La CPU se calcula con TotalProcessorTime delta / elapsed / núcleos (=% de 1 núcleo).
 */
const http = require('http');
const { spawnSync } = require('child_process');

const CDP_PORT = 9223;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function getJson(path) {
  return new Promise((resolve, reject) => {
    http.get({ host: '127.0.0.1', port: CDP_PORT, path }, (res) => {
      let body = '';
      res.on('data', (c) => (body += c));
      res.on('end', () => { try { resolve(JSON.parse(body)); } catch (e) { reject(e); } });
    }).on('error', reject);
  });
}

async function connect() {
  const targets = await getJson('/json/list');
  const page = targets.find((t) => t.type === 'page');
  if (!page) throw new Error('sin página del widget en CDP');
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(page.webSocketDebuggerUrl);
    let id = 0;
    const pending = new Map();
    ws.onopen = () => resolve({
      eval: (expr) => new Promise((res2, rej2) => {
        const mid = ++id;
        pending.set(mid, { res: res2, rej: rej2 });
        ws.send(JSON.stringify({ id: mid, method: 'Runtime.evaluate', params: { expression: expr, returnByValue: true, awaitPromise: true } }));
      }),
      close: () => ws.close(),
    });
    ws.onmessage = (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && pending.has(msg.id)) {
        const p = pending.get(msg.id);
        pending.delete(msg.id);
        if (msg.error) p.rej(new Error(msg.error.message));
        else p.res(msg.result?.result?.value);
      }
    };
    ws.onerror = reject;
  });
}

/** CPU % (de 1 núcleo) y RAM MB del árbol completo del PID raíz, durante `secs`. */
function measureTree(rootPid, secs) {
  const script = `
$ErrorActionPreference = 'SilentlyContinue'
$root = ${rootPid}
$cores = [Environment]::ProcessorCount
$all = Get-CimInstance Win32_Process
$tree = New-Object System.Collections.Generic.HashSet[int]
[void]$tree.Add($root)
$changed = $true
while ($changed) {
  $changed = $false
  foreach ($p in $all) {
    if ($tree.Contains([int]$p.ParentProcessId) -and -not $tree.Contains([int]$p.ProcessId)) {
      [void]$tree.Add([int]$p.ProcessId); $changed = $true
    }
  }
}
function TreeCpu {
  ($tree | ForEach-Object { (Get-Process -Id $_ -ErrorAction SilentlyContinue).TotalProcessorTime.TotalMilliseconds } |
    Measure-Object -Sum).Sum
}
function TreeRam {
  (($tree | ForEach-Object { (Get-Process -Id $_ -ErrorAction SilentlyContinue).WorkingSet64 } |
    Measure-Object -Sum).Sum) / 1MB
}
$t0 = TreeCpu; $ram0 = TreeRam
Start-Sleep -Seconds ${secs}
$t1 = TreeCpu; $ram1 = TreeRam
$pct = ($t1 - $t0) / (${secs} * 1000 * $cores) * 100
Write-Output ("CPU_PCT={0:N2}" -f $pct)
Write-Output ("RAM_MB={0:N1}" -f $ram1)
`;
  const out = spawnSync('powershell', ['-NoProfile', '-Command', script], { encoding: 'utf8', timeout: (secs + 30) * 1000 });
  const cpu = /CPU_PCT=([\d.]+)/.exec(out.stdout)?.[1];
  const ram = /RAM_MB=([\d.]+)/.exec(out.stdout)?.[1];
  return { cpuPct: cpu ? Number(cpu) : null, ramMb: ram ? Number(ram) : null };
}

(async () => {
  console.log('== Medición de impacto real (árbol completo: host Rust + WebView2) ==\n');
  const cdp = await connect();

  // Estado base: visible + modo Gráficas (donde viven las 3 features juntas).
  const apiShape = await cdp.eval(`Object.keys(window.api).sort().join(',')`);
  console.log('api methods:', apiShape, '\n');
  await cdp.eval(`window.api.showWidget?.(); window.api.setWidgetMode('charts'); 'ok'`);
  await sleep(8000); // asentar: llenar historial de disco + un par de ticks

  const sanity = await cdp.eval(`(() => {
    const c = window.Chart?.getChart ? window.Chart.getChart(document.getElementById('chart-disk')) : null;
    return {
      innerH: window.innerHeight,
      diskSeries: c?.data?.datasets?.length ?? 0,
      diskPts: c?.data?.datasets?.[0]?.data?.length ?? 0,
      sessionCard: !!document.querySelector('.card--session'),
    };
  })()`);
  console.log('Sanity (modo Gráficas):', JSON.stringify(sanity), '\n');

  console.log('A) VISIBLE en Gráficas — midiendo 30 s...');
  const vis = measureTree(Number(process.argv[2] || 0), 30);
  console.log(`   CPU visible: ${vis.cpuPct}% de 1 núcleo | RAM: ${vis.ramMb} MB\n`);

  console.log('B) Ocultando a bandeja y midiendo 20 s...');
  await cdp.eval(`window.api.hideWidget()`);
  await sleep(2000);
  const hid = measureTree(Number(process.argv[2] || 0), 20);
  console.log(`   CPU oculto: ${hid.cpuPct}% de 1 núcleo | RAM: ${hid.ramMb} MB\n`);

  await cdp.eval(`window.api.showWidget()`);
  console.log('Widget restaurado. Medición completa.');
  cdp.close();
})().catch((e) => { console.error('FALLO:', e.message); process.exit(1); });
