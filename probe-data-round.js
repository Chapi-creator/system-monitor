'use strict';
/**
 * probe-data-round.js — Sonda E2E de la ronda de datos contra la app viva:
 *  1. Tarjeta SESIÓN (máx/prom desde el arranque) en modo Gráficas.
 *  2. Fila DISK (lectura/escritura B/s) en modo Dev.
 *  3. Tooltip del Top-5 con PID + ruta del exe (o degradación honesta).
 */
const http = require('http');

const CDP_PORT = 9223;
let ok = 0, fail = 0;
function check(name, cond, detail) {
  if (cond) { ok++; console.log(`  ✅ ${name}${detail ? ` — ${detail}` : ''}`); }
  else { fail++; console.log(`  ❌ ${name}${detail ? ` — ${detail}` : ''}`); }
}

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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  console.log('== Sonda ronda de datos (app viva vía CDP) ==\n');
  const cdp = await connect();

  // --- 1. DISK en modo Dev ---
  console.log('1) Métricas de DISK (modo Dev):');
  await cdp.eval(`window.api.setWidgetMode('dev')`);
  await sleep(6000); // > 1 ciclo de stats (2.5 s) + 1er renglón PDH de disco (~2 s)
  const disk = await cdp.eval(`(() => {
    const r = document.getElementById('disk-read')?.textContent;
    const w = document.getElementById('disk-write')?.textContent;
    const api = window.api.getSystemStats ? 'sí' : 'no';
    return { r, w, api };
  })()`);
  check('commando getSystemStats disponible en el puente', disk?.api === 'sí');
  const diskFmt = /^(n\/a|-?[\d.]+ (KB|MB)\/s)$/;
  check('DISK lectura con formato válido', diskFmt.test(disk?.r ?? ''), `read="${disk?.r}"`);
  check('DISK escritura con formato válido', diskFmt.test(disk?.w ?? ''), `write="${disk?.w}"`);

  // --- 2. Tooltip del Top-5 (modo procs) ---
  console.log('\n2) Tooltip por proceso (modo Procesos):');
  await cdp.eval(`window.api.setWidgetMode('procs')`);
  await sleep(4000);
  const tip = await cdp.eval(`(() => {
    const li = document.querySelector('#process-list-full li.process-item:not([hidden])');
    if (!li) return null;
    return { title: li.title || '', rows: li.title ? li.title.split('\\n').length : 0 };
  })()`);
  check('hay al menos una fila visible del Top-5', Boolean(tip));
  check('tooltip presente con 4 líneas (nombre/PID/ruta/CPU-RAM)', (tip?.rows ?? 0) === 4, `${tip?.rows ?? 0} líneas`);
  check('tooltip incluye "PID:"', /PID: \d+/.test(tip?.title ?? ''), (tip?.title ?? '').split('\\n')[1] ?? '');
  const firstLine = (tip?.title ?? '').split('\n')[2] ?? '';
  check('tooltip incluye ruta del exe o degradación honesta',
    firstLine.includes('…') || firstLine.includes(':\\\\') || firstLine === 'ruta no disponible',
    firstLine);

  // --- 3. Tarjeta de SESIÓN (modo charts) ---
  console.log('\n3) Estadísticas de sesión (modo Gráficas):');
  await cdp.eval(`window.api.setWidgetMode('charts')`);
  await sleep(3500); // al menos un pollTick en charts → fetchSessionStats
  const sess = await cdp.eval(`(() => {
    const q = (s) => document.querySelector(s)?.textContent ?? null;
    const api = window.api.getSessionStats ? window.api.getSessionStats() : null;
    return {
      cpuMax: q('.session-cpu-max'), cpuAvg: q('.session-cpu-avg'),
      ramMax: q('.session-ram-max'), gpuMax: q('.session-gpu-max'),
      netMax: q('.session-net-max'), foot: q('.session-foot'),
      apiOk: Boolean(api), cardVisible: Boolean(document.querySelector('.card--session')),
    };
  })()`);
  check('getSessionStats expuesta en el puente', sess?.apiOk === true);
  check('tarjeta SESIÓN presente en el modo Gráficas', sess?.cardVisible === true);
  const pctFmt = /^-?[\d.]+%$/;
  check('CPU máx con formato %', pctFmt.test(sess?.cpuMax ?? ''), `cpuMax="${sess?.cpuMax}"`);
  check('CPU prom con formato %', pctFmt.test(sess?.cpuAvg ?? ''), `cpuAvg="${sess?.cpuAvg}"`);
  check('RAM máx con formato %', pctFmt.test(sess?.ramMax ?? ''), `ramMax="${sess?.ramMax}"`);
  check('GPU máx con formato %', pctFmt.test(sess?.gpuMax ?? ''), `gpuMax="${sess?.gpuMax}"`);
  check('RED máx con formato velocidad', /^[\d.]+ (KB|MB)\/s$/.test(sess?.netMax ?? ''), `netMax="${sess?.netMax}"`);
  check('pie con muestras acumuladas', /desde el arranque · \d+ muestras/.test(sess?.foot ?? ''), sess?.foot ?? '');
  const api = await cdp.eval(`window.api.getSessionStats()`);
  check('API de sesión coherente (samples > 0 y máximos numéricos)',
    api?.ok === true && api?.samples > 0 && Number.isFinite(api?.cpu?.max),
    `samples=${api?.samples} cpuMax=${api?.cpu?.max}`);
  check('máximos ≥ promedios (invariante)',
    api.cpu.max >= api.cpu.avg && api.mem.max >= api.mem.avg && api.net.max >= api.net.avg);

  await cdp.eval(`window.api.setWidgetMode('dev')`);
  cdp.close();
  console.log(`\nRESULTADO ronda de datos: ${ok} OK, ${fail} FAIL`);
  process.exit(fail === 0 ? 0 : 1);
})().catch((e) => { console.error('ERROR:', e.message); process.exit(1); });
