'use strict';
/**
 * measure-impact.js — Medición A/B del impacto real en CPU/RAM.
 *
 * Uso: node measure-impact.js <ruta-exe> <tag>
 *
 * Lanza el exe con CDP (puerto 9224), mide el ÁRBOL COMPLETO de procesos
 * (host Rust + WebView2) vía PowerShell (TotalProcessorTime delta) en:
 *   - dev visible 30 s, charts visible 30 s, procs visible 30 s
 *   - oculto en bandeja 20 s (desde charts)
 * Escribe impact-<tag>-<ts>.json con los resultados.
 */
const { spawn, spawnSync } = require('child_process');
const http = require('http');
const fs = require('fs');
const os = require('os');
const path = require('path');

const exe = path.resolve(process.argv[2]);
const tag = process.argv[3] || 'run';
const CDP_PORT = 9224;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function httpJson(port, p) {
  return new Promise((resolve, reject) => {
    http.get({ host: '127.0.0.1', port, path: p }, (res) => {
      let b = ''; res.on('data', (c) => (b += c)); res.on('end', () => resolve(JSON.parse(b)));
    }).on('error', reject);
  });
}

async function waitCdp() {
  for (let i = 0; i < 40; i++) {
    try { return await httpJson(CDP_PORT, '/json/version'); } catch { await sleep(500); }
  }
  throw new Error('CDP no respondió en 20 s');
}

function connect(port) {
  return httpJson(port, '/json/list').then((targets) => {
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
  });
}

function killByPath() {
  spawnSync('powershell', ['-NoProfile', '-Command',
    `Get-Process | Where-Object { $_.Path -eq '${exe.replace(/'/g, "''")}' } | Stop-Process -Force; Start-Sleep 1`]);
}

function findRootPid() {
  const out = spawnSync('powershell', ['-NoProfile', '-Command',
    `(Get-Process | Where-Object { $_.Path -eq '${exe.replace(/'/g, "''")}' } | Select-Object -First 1).Id`],
    { encoding: 'utf8', timeout: 20000 });
  const pid = Number(out.stdout.trim());
  if (!pid) throw new Error('no encontré el proceso del widget');
  return pid;
}

/** CPU % (de 1 núcleo) y RAM MB del árbol completo durante `secs`. */
function psMeasure(rootPid, secs) {
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
Write-Output ("CPU_PCT={0:N2}" -f (($t1 - $t0) / (${secs} * 1000 * $cores) * 100))
Write-Output ("RAM_MB={0:N1}" -f $ram1)
`;
  const out = spawnSync('powershell', ['-NoProfile', '-Command', script],
    { encoding: 'utf8', timeout: (secs + 30) * 1000 });
  const cpu = /CPU_PCT=([\d.]+)/.exec(out.stdout || '')?.[1];
  const ram = /RAM_MB=([\d.]+)/.exec(out.stdout || '')?.[1];
  if (!cpu) throw new Error(`PowerShell no midió CPU: ${out.stderr}`);
  return { cpuPct: Number(cpu), ramMb: ram ? Number(ram) : null };
}

(async () => {
  console.log(`== Medición [${tag}] ==\n  exe: ${exe}\n`);
  killByPath();

  const udf = path.join(os.tmpdir(), `wv2-impact-${tag}`);
  const child = spawn(exe, [], {
    env: {
      ...process.env,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
      WEBVIEW2_USER_DATA_FOLDER: udf,
    },
    stdio: 'ignore',
  });

  await waitCdp();
  const rootPid = findRootPid();
  console.log(`PID raíz: ${rootPid}\n`);
  const cdp = await connect(CDP_PORT);
  const results = { tag, exe, rootPid, scenarios: {} };

  for (const [mode, secs] of [['dev', 30], ['charts', 30], ['procs', 30]]) {
    await cdp.eval(`window.api.setWidgetMode('${mode}')`);
    await sleep(6000); // asentar: ticks + (en el nuevo) historial de disco
    const r = psMeasure(rootPid, secs);
    results.scenarios[`${mode}-visible`] = r;
    console.log(`${mode.padEnd(7)} visible ${secs}s → CPU ${r.cpuPct}% de 1 núcleo | RAM ${r.ramMb} MB`);
  }

  // Oculto en bandeja: el gating debe llevar la CPU a ~0.
  await cdp.eval(`window.api.setWidgetMode('charts')`);
  await sleep(2000);
  await cdp.eval(`window.api.hideWidget()`);
  await sleep(2000);
  const rh = psMeasure(rootPid, 20);
  results.scenarios['hidden'] = rh;
  console.log(`${'oculto'.padEnd(7)} en bandeja 20s → CPU ${rh.cpuPct}% de 1 núcleo | RAM ${rh.ramMb} MB`);
  await cdp.eval(`window.api.showWidget()`);

  const outFile = `impact-${tag}-${Date.now()}.json`;
  fs.writeFileSync(outFile, JSON.stringify(results, null, 2));
  console.log(`\nResultados → ${outFile}`);

  cdp.close();
  killByPath();
  process.exit(0);
})().catch((e) => { console.error('FALLO:', e.message); killByPath(); process.exit(1); });
