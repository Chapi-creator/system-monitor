'use strict';
// E2E: verificar que onModeChanged llega VIVO al renderer.
//
// Cubre tres rutas:
//   A. Cambio de modo por IPC directa (setWidgetMode) → el backend emite
//      'mode-changed' y el renderer sigue (el bug de desincronización original).
//   B. Cambio de modo por UI (clic en botón) → el loop UI→Rust→evento→renderer
//      cierra con EXACTAMENTE un evento por transición (sin eco duplicado).
//   C. unlisten → el renderer deja de recibir eventos; el estado se restaura
//      por UI (que no depende del listener).
//
// Requisito: app corriendo con CDP en 9223 (WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS).

const PORT = 9223;
let passed = 0;
let failed = 0;
const fails = [];

function check(name, cond, extra = '') {
  if (cond) { passed++; console.log(`  ✅ ${name}${extra ? ' — ' + extra : ''}`); }
  else { failed++; fails.push(name); console.log(`  ❌ ${name}${extra ? ' — ' + extra : ''}`); }
}

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
  // Espera activa hasta que la condición se cumpla (tope 6 s).
  const waitFor = async (expr, desc) => {
    const start = Date.now();
    while (Date.now() - start < 6000) {
      if (await evalJs(expr)) return true;
      await sleep(150);
    }
    return false;
  };

  // Suscripción en la página: colector de eventos + guardado del unlisten id.
  await evalJs(`(() => {
    window.__modeEvents = [];
    return window.api.onModeChanged((mode) => window.__modeEvents.push(mode))
      .then((eventId) => { window.__modeUnlistenId = eventId; return 'subscribed'; });
  })()`);
  const mode = () => evalJs(`document.querySelector('.widget')?.dataset.mode || null`);
  const events = () => evalJs(`window.__modeEvents.slice()`);

  console.log('=== A. IPC directa → evento → renderer sigue ===');
  const baseline = await mode();
  check('modo inicial dev (bootstrap)', baseline === 'dev', `data-mode=${baseline}`);
  await evalJs(`window.api.setWidgetMode('charts')`); // backend-driven, SIN pasar por setMode
  const gotCharts = await waitFor(`window.__modeEvents.includes('charts') && document.querySelector('.widget').dataset.mode === 'charts'`, 'evento charts + UI');
  check('evento mode-changed recibido en vivo', gotCharts, `eventos=${JSON.stringify(await events())}`);
  check('UI del renderer siguió al backend (data-mode=charts)', (await mode()) === 'charts');
  await evalJs(`window.api.setWidgetMode('mini')`);
  const gotMini = await waitFor(`window.__modeEvents.filter(m => m === 'mini').length === 1 && document.querySelector('.widget').dataset.mode === 'mini'`, 'evento mini + UI');
  check('segunda transición también llega (mini)', gotMini, `eventos=${JSON.stringify(await events())}`);

  console.log('\n=== B. Clic UI → un solo evento (sin eco) ===');
  const before = (await events()).length;
  await evalJs(`document.getElementById('btn-dev').click()`);
  const uiSettled = await waitFor(`document.querySelector('.widget').dataset.mode === 'dev' && window.__modeEvents.length === ${before} + 1`, 'UI→evento único');
  check('transición UI produce exactamente 1 evento', uiSettled, `eventos=${JSON.stringify(await events())} (esperados ${(await events()).length})`);
  check('modo aplicado por la UI', (await mode()) === 'dev');

  console.log('\n=== C. unlisten corta la suscripción ===');
  await evalJs(`window.__TAURI_INTERNALS__.invoke('plugin:event|unlisten', { event: 'mode-changed', eventId: window.__modeUnlistenId }).then(() => 'unlistened')`);
  const beforeUn = (await events()).length;
  await evalJs(`window.api.setWidgetMode('mini')`); // el backend emite, nadie escucha
  await sleep(2000);
  const afterUn = (await events()).length;
  check('sin eventos tras unlisten', afterUn === beforeUn, `antes=${beforeUn} después=${afterUn}`);
  // Restaurar por UI (no depende del listener desechado).
  await evalJs(`document.getElementById('btn-dev').click()`);
  await waitFor(`document.querySelector('.widget').dataset.mode === 'dev'`, 'restaurar dev');
  check('restaurado a dev por UI', (await mode()) === 'dev');

  ws.close();
  console.log(`\n========== RESULTADO E2E onModeChanged: ${passed} OK, ${failed} FAIL ==========`);
  if (fails.length) console.log('Fallos:', fails.join(', '));
  process.exit(failed ? 1 : 0);
}

main().catch((e) => { console.error(e); process.exit(1); });
