'use strict';
// Test integral del widget Tauri: cubre IPC, 4 modos, métricas, kill-process,
// single instance, ocultar-a-bandeja, always-on-top y alertas.
const { execSync, spawn } = require('node:child_process');
const PORT = 9223;
let passed = 0;
let failed = 0;
const fails = [];

function check(name, cond, extra = '') {
  if (cond) { passed++; console.log(`  ✅ ${name}${extra ? ' — ' + extra : ''}`); }
  else { failed++; fails.push(name); console.log(`  ❌ ${name}${extra ? ' — ' + extra : ''}`); }
}

const ps = (cmd) => {
  try { return execSync(cmd, { encoding: 'utf8', windowsHide: true, timeout: 20000 }).trim(); }
  catch { return ''; }
};

async function main() {
  const targets = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
  const page = targets.find((t) => t.type === 'page');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  let id = 0;
  const pending = new Map();
  const send = (method, params = {}) =>
    new Promise((resolve) => { const i = ++id; pending.set(i, resolve); ws.send(JSON.stringify({ id: i, method, params })); });
  ws.onmessage = (ev) => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } };
  await new Promise((r) => (ws.onopen = r));
  const evalJs = async (expr) => {
    const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    return r.result?.result?.value;
  };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  console.log('=== 1. Superficie IPC ===');
  const apiShape = await evalJs(`Object.keys(window.api || {}).sort().join(',')`);
  check('window.api con 12 métodos', apiShape === 'getAlwaysOnTop,getGpuInfo,getSessionStats,getSystemStats,getTopProcesses,hideWidget,killProcess,onModeChanged,onVisibilityChanged,setWidgetMode,showWidget,toggleAlwaysOnTop', apiShape);

  console.log('\n=== 2. Métricas en vivo (get-system-stats) ===');
  const s1 = await evalJs(`window.api.getSystemStats().then(s => s)`);
  check('ok=true', s1?.ok === true);
  check('cpu en [0,100]', typeof s1?.cpu === 'number' && s1.cpu >= 0 && s1.cpu <= 100, `${s1.cpu}%`);
  check('ram.percent en (0,100]', typeof s1?.memory?.percent === 'number' && s1.memory.percent > 0, `${s1.memory.percent}%`);
  check('ram.usedGb/totalGb coherentes', s1?.memory?.usedGb > 0 && s1?.memory?.totalGb > s1?.memory?.usedGb, `${s1.memory.usedGb}/${s1.memory.totalGb} GB`);
  check('network.iface no n/a', typeof s1?.network?.iface === 'string' && s1.network.iface !== 'n/a', s1.network.iface);
  check('tempStatus en [ok,admin,none]', ['ok', 'admin', 'none'].includes(s1?.tempStatus), s1.tempStatus);
  check('gpu número (-1 = sin dato)', typeof s1?.gpu === 'number', `${s1.gpu}%`);

  console.log('\n=== 3. Modo Dev: tarjetas pobladas ===');
  await evalJs(`window.api.setWidgetMode('dev')`);
  await sleep(1200);
  const cards = await evalJs(`(() => {
    const g = (id) => { const el = document.getElementById(id); return el ? el.textContent.trim() : null; };
    return { cpu: g('cpu-value'), ram: g('ram-value'), gpu: g('gpu-value'), temp: g('temp-value'), netDown: g('net-down'), netUp: g('net-up'), netIface: g('net-iface') };
  })()`);
  check('tarjeta CPU', cards?.cpu && cards.cpu !== '--' && cards.cpu !== 'n/a', cards?.cpu);
  check('tarjeta RAM', cards?.ram && cards.ram !== '--', cards?.ram);
  check('tarjeta GPU', cards?.gpu && cards.gpu !== '--' && cards.gpu !== 'n/a', cards?.gpu);
  check('tarjeta NET (iface + down/up)', cards?.netIface && cards.netIface !== 'n/a' && cards.netDown && cards.netDown !== '--' && cards.netUp && cards.netUp !== '--', `${cards?.netIface} ↓${cards?.netDown} ↑${cards?.netUp}`);
  const gpuDetail = await evalJs(`document.getElementById('gpu-detail')?.textContent.trim()`);
  check('detalle GPU (modelo · VRAM)', /Intel|NVIDIA|AMD|Radeon|GeForce/i.test(gpuDetail || ''), gpuDetail);

  console.log('\n=== 4. Modo Gráficas: Chart.js + 6 canvas ===');
  await evalJs(`window.api.setWidgetMode('charts')`);
  await sleep(1500);
  const charts = await evalJs(`(() => {
    const canvases = [...document.querySelectorAll('canvas')];
    const sized = canvases.filter(c => c.width > 0 && c.height > 0).length;
    return { total: canvases.length, sized, chartJs: typeof window.Chart === 'function', visible: [...document.querySelectorAll('.mode')].find(m => !m.hidden)?.id };
  })()`);
  check('Chart.js cargado', charts?.chartJs === true);
  check('6 canvas con tamaño real', charts?.sized === 6, `${charts?.sized}/6`);
  const chartValues = await evalJs(`(() => {
    const charts = window.Chart.getChart ? window.Chart.getChart(document.getElementById('chart-cpu')) : null;
    const ds = charts?.data?.datasets?.[0]?.data || [];
    return { n: ds.length, hasValue: ds.some(v => typeof v === 'number' && v > 0) };
  })()`);
  check('serie CPU con datos', chartValues?.hasValue === true, `${chartValues?.n} puntos`);
  const diskChartProbe = await evalJs(`(() => {
    const c = window.Chart.getChart ? window.Chart.getChart(document.getElementById('chart-disk')) : null;
    const rd = c?.data?.datasets?.[0]?.data || [];
    const wr = c?.data?.datasets?.[1]?.data || [];
    return { hasRead: rd.some(v => typeof v === 'number' && v >= 0), hasWrite: wr.some(v => typeof v === 'number' && v >= 0), n: rd.length };
  })()`);
  check('gráfica DISCO: 2 series con datos', diskChartProbe?.hasRead === true && diskChartProbe?.hasWrite === true, `${diskChartProbe?.n} puntos`);

  console.log('\n=== 5. Modo Mini: 4 strips + red ===');
  await evalJs(`window.api.setWidgetMode('mini')`);
  await sleep(3000); // > intervalo de polling (2.5 s): garantiza un tick en mini
  const mini = await evalJs(`(() => {
    const g = (id) => { const el = document.getElementById(id); return el ? el.textContent.trim() : null; };
    return { cpu: g('mini-cpu-value'), ram: g('mini-ram-value'), temp: g('mini-temp-value'), gpu: g('mini-gpu-value'), net: g('mini-net-iface'), down: g('mini-net-down'), up: g('mini-net-up') };
  })()`);
  check('mini CPU', mini?.cpu && mini.cpu !== '--', mini?.cpu);
  check('mini RAM', mini?.ram && mini.ram !== '--', mini?.ram);
  check('mini GPU', mini?.gpu && mini.gpu !== '--' && mini.gpu !== 'n/a', mini?.gpu);
  check('mini red (interfaz + down/up)', mini?.net && mini.net !== 'n/a', `${mini.net} ↓${mini.down} ↑${mini.up}`);

  console.log('\n=== 6. Modo Procesos: top-5 ===');
  await evalJs(`window.api.setWidgetMode('procs')`);
  await sleep(3000); // > intervalo de polling (2.5 s) + refresh de procesos
  const procs = await evalJs(`document.querySelectorAll('#process-list-full li.process-item').length`);
  const procsText = await evalJs(`document.getElementById('process-list-full')?.textContent.trim().slice(0, 120)`);
  check('5 filas de procesos', procs === 5, `filas=${procs} → ${procsText}`);
  const topProc = await evalJs(`window.api.getTopProcesses().then(r => r.processes?.[0])`);
  check('top proceso con pid/name/cpu/mem', topProc?.pid > 0 && topProc?.name && typeof topProc?.cpu === 'number', `${topProc?.name} (${topProc?.pid}) cpu=${topProc?.cpu}`);

  console.log('\n=== 7. Comportamientos de ventana ===');
  // 7a. setWidgetMode redimensiona (dev 340x440 → mini 340x245)
  await evalJs(`window.api.setWidgetMode('dev')`);
  await sleep(800);
  const hDev = await evalJs(`window.innerHeight`);
  await evalJs(`window.api.setWidgetMode('mini')`);
  await sleep(800);
  const hMini = await evalJs(`window.innerHeight`);
  check('resize dev→mini reduce alto', hMini < hDev, `dev=${hDev}px → mini=${hMini}px`);
  await evalJs(`window.api.setWidgetMode('dev')`);
  await sleep(500);

  // 7b. always-on-top toggle
  const pin1 = await evalJs(`window.api.toggleAlwaysOnTop().then(r => r.pinned)`);
  const pin2 = await evalJs(`window.api.getAlwaysOnTop().then(r => r.pinned)`);
  check('toggle always-on-top (true→false)', pin1 === false && pin2 === false, `pinned=${pin2}`);
  const pin3 = await evalJs(`window.api.toggleAlwaysOnTop().then(r => r.pinned)`);
  check('toggle always-on-top (false→true)', pin3 === true);

  // 7c. ocultar a bandeja: window.close() está remapeado a hide_widget en el
  // puente (wry destruiría el HWND del webview → ventana negra pegada).
  // El proceso debe seguir vivo y el documento pasar a hidden.
  const beforePid = await evalJs(`window.api.getSystemStats().then(() => 'alive')`);
  check('app viva antes de close', beforePid === 'alive');
  await evalJs(`window.close()`);
  await sleep(1500);
  const stillRunning = ps(`powershell -NoProfile -Command "(Get-Process -Name 'System Monitor Widget-Tauri-Portable-1.0.0' -ErrorAction SilentlyContinue).Count"`);
  check('window.close() → proceso sigue vivo (va a bandeja)', stillRunning === '1', `procesos=${stillRunning}`);
  // document.hidden NO refleja el ocultado del host en WebView2: la verdad
  // está a nivel OS (IsWindowVisible sobre la clase 'Tauri Window').
  const osHidden = ps(`powershell -NoProfile -ExecutionPolicy Bypass -File win-visibility.ps1`);
  check('ventana oculta a nivel OS tras close (no negra ni destruida)', osHidden === 'MAIN_VISIBLE=False', osHidden);
  await evalJs(`window.api.hideWidget().then(() => 'ok')`);
  await sleep(500);
  const osHidden2 = ps(`powershell -NoProfile -ExecutionPolicy Bypass -File win-visibility.ps1`);
  check('hideWidget explícito también oculta a nivel OS', osHidden2 === 'MAIN_VISIBLE=False', osHidden2);
  const aliveAfterHide = await evalJs(`window.api.getSystemStats().then(() => 'alive')`);
  check('hideWidget/showWidget no destruye el webview', aliveAfterHide === 'alive');
  await evalJs(`window.api.showWidget().then(() => 'ok')`);
  await sleep(500);
  const osShown = ps(`powershell -NoProfile -ExecutionPolicy Bypass -File win-visibility.ps1`);
  check('showWidget restaura la ventana', osShown === 'MAIN_VISIBLE=True', osShown);

  console.log('\n=== 8. kill-process (proceso dummy) ===');
  const dummy = spawn('node', ['-e', 'setInterval(()=>{},1000);'], { windowsHide: true, stdio: 'ignore' });
  await sleep(800);
  const aliveBefore = ps(`powershell -NoProfile -Command "(Get-Process -Id ${dummy.pid} -ErrorAction SilentlyContinue).Count"`);
  check('proceso dummy vivo antes', aliveBefore === '1', `pid=${dummy.pid}`);
  const killRes = await evalJs(`window.api.killProcess(${dummy.pid}).then(r => r)`);
  check('killProcess ok=true', killRes?.ok === true, JSON.stringify(killRes));
  await sleep(1200);
  const aliveAfter = ps(`powershell -NoProfile -Command "(Get-Process -Id ${dummy.pid} -ErrorAction SilentlyContinue).Count"`);
  check('proceso dummy muerto después', aliveAfter === '0', `vivos=${aliveAfter}`);
  const killInvalid = await evalJs(`window.api.killProcess(-5).then(r => r)`);
  check('killProcess PID inválido rechazado', killInvalid?.ok === false, JSON.stringify(killInvalid));

  console.log('\n=== 9. Instancia única ===');
  const beforeInstances = ps(`powershell -NoProfile -Command "(Get-Process -Name 'System Monitor Widget-Tauri-Portable-1.0.0' -ErrorAction SilentlyContinue).Count"`);
  const second = spawn('"./dist/System Monitor Widget-Tauri-Portable-1.0.0.exe"', [], { shell: true, windowsHide: true, stdio: 'ignore' });
  await sleep(4000);
  const afterInstances = ps(`powershell -NoProfile -Command "(Get-Process -Name 'System Monitor Widget-Tauri-Portable-1.0.0' -ErrorAction SilentlyContinue).Count"`);
  check('segundo lanzamiento no duplica proceso', afterInstances === '1', `antes=${beforeInstances} después=${afterInstances}`);
  try { second.kill(); } catch { /* ya salió solo */ }

  console.log('\n=== 10. Alertas: notificación única, sin banner ===');
  await evalJs(`(() => {
    if (window.__tSpy) return;
    const Orig = window.Notification;
    window.__tSpy = { toasts: [], bannerSeen: false };
    window.Notification = class Spy extends Orig {
      constructor(title, opts) { super(title, opts); window.__tSpy.toasts.push(String(title)); }
    };
    const obs = new MutationObserver(() => {
      if (document.querySelector('.alert-strip, #alert-strip, .strip--alert')) window.__tSpy.bannerSeen = true;
    });
    obs.observe(document.body, { childList: true, subtree: true, attributes: true });
  })()`);
  const load = spawn('node', ['load-alerts.js', 'cpu'], { windowsHide: true, stdio: 'ignore' });
  const start = Date.now();
  let fired = false;
  while (Date.now() - start < 45000) {
    await sleep(4000);
    const spy = await evalJs(`({ n: window.__tSpy.toasts.length, t: window.__tSpy.toasts, banner: window.__tSpy.bannerSeen })`);
    if (spy.n >= 1) { fired = true; check('notificación nativa disparada', spy.n === 1, spy.t.join(' | ')); break; }
  }
  load.kill();
  const spyFinal = await evalJs(`({ n: window.__tSpy.toasts.length, t: window.__tSpy.toasts, banner: window.__tSpy.bannerSeen })`);
  if (!fired) check('notificación nativa disparada', spyFinal.n >= 1, JSON.stringify(spyFinal));
  check('sin banner interno (franja LED)', spyFinal?.banner === false);
  check('exactamente una notificación por episodio', spyFinal?.n === 1, `toasts=${spyFinal?.n}`);

  ws.close();
  console.log(`\n========== RESULTADO FINAL: ${passed} OK, ${failed} FAIL ==========`);
  if (fails.length) console.log('Fallos:', fails.join(', '));
  process.exit(failed ? 1 : 0);
}

main().catch((e) => { console.error(e); process.exit(1); });