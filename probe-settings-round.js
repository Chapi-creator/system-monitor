'use strict';
/**
 * probe-settings-round.js — Sonda E2E de la ronda de ajustes contra la app viva:
 *  1. getSettings devuelve settings completos (mode, pinned, thresholds).
 *  2. setThreshold ajusta el umbral en vivo y el guardián JS lo adopta.
 *  3. El overlay de Ajustes existe y repinta valores.
 *  4. Persistencia: el JSON de %APPDATA% refleja el cambio.
 */
const http = require('http');
const fs = require('fs');

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
  console.log('== Sonda ronda de ajustes (app viva vía CDP) ==\n');
  const cdp = await connect();

  // 1. API de settings
  const shape = await cdp.eval(`Object.keys(window.api||{}).filter(k=>/settings|threshold/i.test(k)).sort().join(',')`);
  check('window.api expone getSettings/setThreshold/setThresholdDelta/onThresholdsChanged', shape === 'getSettings,onThresholdsChanged,setThreshold,setThresholdDelta', shape);

  // 2. getSettings completo
  const st = await cdp.eval(`window.api.getSettings().then(r => JSON.stringify(r))`).then(JSON.parse);
  check('getSettings → ok:true', st?.ok === true);
  const th = st?.settings?.thresholds;
  check('settings.thresholds con cpu/ram/gpu/temp', th && ['cpu','ram','gpu','temp'].every(k => typeof th[k] === 'number'), JSON.stringify(th));
  check('settings.mode es un modo válido', ['dev','mini','charts','procs'].includes(st?.settings?.mode), st?.settings?.mode);
  check('settings.pinned es boolean', typeof st?.settings?.pinned === 'boolean');

  // 3. setThreshold en vivo: sube CPU a 95 y verifica eco en el guardián
  const origCpu = th.cpu;
  const up = await cdp.eval(`window.api.setThreshold('cpu', 95).then(r => JSON.stringify(r))`).then(JSON.parse);
  check('setThreshold(cpu,95) → ok:true', up?.ok === true && up?.thresholds?.cpu === 95, JSON.stringify(up?.thresholds));
  await sleep(300);
  const adopted = await cdp.eval(`THRESHOLDS.cpu`);
  check('el guardián JS adoptó el umbral nuevo', adopted === 95, `THRESHOLDS.cpu=${adopted}`);

  // 4. Persistencia real en disco
  const os = require('os');
  const path = require('path');
  const file = path.join(os.homedir(), 'AppData', 'Roaming', 'SystemMonitorWidget', 'settings.json');
  const saved = JSON.parse(fs.readFileSync(file, 'utf8'));
  check('settings.json persistió cpu=95', saved?.thresholds?.cpu === 95, `cpu=${saved?.thresholds?.cpu}`);

  // 5. Clamp: valor absurdo → acotado
  const cl = await cdp.eval(`window.api.setThreshold('cpu', 5000).then(r => JSON.stringify(r))`).then(JSON.parse);
  check('setThreshold(cpu,5000) → clamp a 100', cl?.ok === true && cl?.thresholds?.cpu === 100, `cpu=${cl?.thresholds?.cpu}`);

  // 6. Restaurar el original
  const back = await cdp.eval(`window.api.setThreshold('cpu', ${origCpu}).then(r => JSON.stringify(r))`).then(JSON.parse);
  check(`restaurado a cpu=${origCpu}`, back?.ok === true && back?.thresholds?.cpu === origCpu);

  // 7. Overlay de Ajustes en el DOM
  const overlay = await cdp.eval(`(() => {
    const ov = document.getElementById('settings-overlay');
    if (!ov) return 'missing';
    return JSON.stringify({
      exists: true,
      hidden: ov.hidden,
      rows: ov.querySelectorAll('.settings-row').length,
      steppers: ov.querySelectorAll('button[data-th]').length,
      cpuVal: document.getElementById('th-cpu-val')?.textContent ?? null,
    });
  })()`).then((s) => s === 'missing' ? null : JSON.parse(s));
  check('overlay #settings-overlay presente', overlay?.exists === true);
  check('overlay arranca oculto', overlay?.hidden === true);
  check('4 filas de umbrales con 8 steppers', overlay?.rows === 4 && overlay?.steppers === 8, `rows=${overlay?.rows} steppers=${overlay?.steppers}`);

  // 8. Abrir el overlay con toggleSettings y verificar repintado
  await cdp.eval(`document.getElementById('btn-settings').click()`);
  await sleep(200);
  const open = await cdp.eval(`(() => {
    const ov = document.getElementById('settings-overlay');
    return JSON.stringify({ hidden: ov.hidden, cpuVal: document.getElementById('th-cpu-val')?.textContent });
  })()`).then(JSON.parse);
  check('clic en ⚙ abre el overlay', open?.hidden === false);
  check('overlay repinta el valor vigente de CPU', open?.cpuVal === `${Math.round(origCpu)}%`, `th-cpu-val="${open?.cpuVal}"`);

  // 9. Un stepper −5 ajusta en vivo (y queda persistido)
  await cdp.eval(`document.querySelector('button[data-th="cpu"][data-step="-5"]').click()`);
  await sleep(400);
  const afterStep = await cdp.eval(`THRESHOLDS.cpu`);
  check('stepper −5 ajusta THRESHOLDS en vivo', afterStep === Math.round(origCpu) - 5, `THRESHOLDS.cpu=${afterStep}`);
  const savedStep = JSON.parse(fs.readFileSync(file, 'utf8'));
  check('stepper persistió en settings.json', savedStep?.thresholds?.cpu === Math.round(origCpu) - 5, `cpu=${savedStep?.thresholds?.cpu}`);

  // 10. Cerrar overlay y restaurar todo
  await cdp.eval(`document.getElementById('settings-close').click()`);
  await sleep(150);
  const closed = await cdp.eval(`document.getElementById('settings-overlay').hidden`);
  check('botón ✕ cierra el overlay', closed === true);
  await cdp.eval(`window.api.setThreshold('cpu', ${origCpu})`);
  const fin = JSON.parse(fs.readFileSync(file, 'utf8'));
  check(`estado final restaurado a cpu=${origCpu}`, fin?.thresholds?.cpu === origCpu, `cpu=${fin?.thresholds?.cpu}`);

  cdp.close();
  console.log(`\n== RESULTADO: ${ok} OK, ${fail} FAIL ==`);
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.error('SONDA ERROR:', e.message); process.exit(2); });
