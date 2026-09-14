'use strict';
/**
 * load-alerts.js — Generador de carga sintética para probar las alertas del
 * guardián EN VIVO. Uso:
 *
 *   node load-alerts.js cpu   → satura CPU (~100%) por 35 s   → alerta ⚠️ ≥85%
 *   node load-alerts.js ram   → reserva ~8.5 GB por 25 s      → alerta 🧠 ≥90%
 *   node load-alerts.js gpu   → ventana WebGL estrés 50 s     → alerta 🎮 ≥90%
 *   node load-alerts.js all   → cpu + ram + gpu en paralelo
 *
 * TEMP (≥80 °C): NO es disparable en la mayoría de equipos — el sensor ACPI no
 * está accesible sin elevación (la app muestra 'n/a' + hint), así que no hay
 * dato que llevar por encima del umbral.
 */

const scenario = (process.argv[2] || 'cpu').toLowerCase();
const os = require('node:os');
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');

const log = (msg) => console.log(`[load] ${new Date().toLocaleTimeString()} ${msg}`);

// ---------------------------------------------------------------------------
// CPU: 1 worker por hilo físico, bucle ocupado por N ms.
// ---------------------------------------------------------------------------
function stressCpu(durationMs) {
  const workers = Math.max(1, Math.floor(os.cpus().length * 0.75));
  log(`CPU: saturando ${workers} hilos durante ${durationMs / 1000}s`);
  const workersFactory = require('node:worker_threads');
  const list = [];
  for (let i = 0; i < workers; i++) {
    const w = new workersFactory.Worker(
      `const end = Date.now() + ${durationMs};
       while (Date.now() < end) { Math.sqrt(Math.random()) * Math.random(); }`,
      { eval: true }
    );
    list.push(new Promise((r) => w.on('exit', r)));
  }
  return Promise.all(list);
}

// ---------------------------------------------------------------------------
// RAM: reservar ~80% de la memoria total en buffers grandes y tocarlos.
// ---------------------------------------------------------------------------
function stressRam(durationMs) {
  // 80% del total: el widget mide (total - available), y Windows mantiene
  // standby reclaimable que NO cuenta como uso → hay que reservar fuerte
  // para cruzar el umbral de alerta del 90% en la LECTURA del widget.
  // Reserva COMPLETA ANTES de arrancar la ventana de espera: la rampa gradual
  // se comía la ventana y el umbral solo se tocaba 1 muestra (sin streak).
  const targetBytes = Math.floor(os.totalmem() * 0.8);
  log(`RAM: reservando ${(targetBytes / 1024 ** 3).toFixed(1)} GB (pre-asignado) y reteniendo ${durationMs / 1000}s`);
  const chunks = [];
  const chunkSize = 32 * 1024 * 1024;
  try {
    while (chunks.reduce((s, b) => s + b.length, 0) < targetBytes) {
      const buf = Buffer.alloc(chunkSize, 0xAA);
      for (let i = 0; i < buf.length; i += 4096) buf[i] = i & 0xFF; // commit real
      chunks.push(buf);
    }
  } catch { /* OOM ahí, con lo reservado basta */ }
  log(`RAM: ${((chunks.reduce((s, b) => s + b.length, 0)) / 1024 ** 3).toFixed(1)} GB residentes — manteniendo`);
  return new Promise((r) => setTimeout(() => { chunks.length = 0; r(); }, durationMs));
}

// ---------------------------------------------------------------------------
// GPU: ventana de navegador VISIBLE (modo --app) corriendo un shader WebGL.
// Desde el port a Tauri ya no hay runtime de Electron: se usa Edge (incluido
// en Windows 10/11 vía WebView2) o Chrome. La ventana debe ser visible: las
// ventanas ocultas/privadas las estrangula DWM y la carga se diluye a nada.
// ---------------------------------------------------------------------------
function findBrowser() {
  const candidates = [
    'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
    'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
    'C:/Program Files/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
  ];
  return candidates.find((p) => fs.existsSync(p));
}

async function stressGpu(durationMs) {
  const browser = findBrowser();
  if (!browser) {
    log('GPU: no se encontró Edge/Chrome — escenario no disponible');
    process.exit(2);
  }

  const htmlPath = path.join(os.tmpdir(), 'sysmon-gpu-stress.html');
  const fsShader = `
    precision highp float; uniform vec2 r; void main() {
      vec2 p = gl_FragCoord.xy / r;
      float v = 0.0;
      for (int i = 0; i < 512; i++) { v += sin(p.x * float(i) + cos(p.y * float(i))); }
      gl_FragColor = vec4(v * 0.002, v * 0.001, abs(sin(v)), 1.0);
    }`;
  // setInterval (NO requestAnimationFrame): en fondos bloqueados el RAF se
  // limita a ~1 fps. Múltiples draws por tick + gl.finish() para que la GPU
  // no encadre trabajo.
  const html = `<!DOCTYPE html><html><body style="margin:0"><canvas id="c"></canvas><script>
    const c = document.getElementById('c');
    c.width = 2560; c.height = 1440;
    const gl = c.getContext('webgl2') || c.getContext('webgl');
    if (!gl) { document.title = 'no-webgl'; } else {
      const mk = (t, s) => { const sh = gl.createShader(t); gl.shaderSource(sh, s); gl.compileShader(sh); return sh; };
      const prog = gl.createProgram();
      gl.attachShader(prog, mk(gl.VERTEX_SHADER, 'attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }'));
      gl.attachShader(prog, mk(gl.FRAGMENT_SHADER, ${JSON.stringify(fsShader)}));
      gl.linkProgram(prog); gl.useProgram(prog);
      gl.uniform2f(gl.getUniformLocation(prog, 'r'), 2560, 1440);
      const buf = gl.createBuffer();
      gl.bindBuffer(gl.ARRAY_BUFFER, buf);
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1, 3,-1, -1,3]), gl.STATIC_DRAW);
      const loc = gl.getAttribLocation(prog, 'p');
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
      const end = Date.now() + ${durationMs};
      const timer = setInterval(() => {
        if (Date.now() > end) { clearInterval(timer); document.title = 'done'; return; }
        for (let d = 0; d < 6; d++) gl.drawArrays(gl.TRIANGLES, 0, 3);
        gl.finish();
      }, 12);
    }
  </script></body></html>`;
  fs.writeFileSync(htmlPath, html);
  log(`GPU: estrés WebGL ${durationMs / 1000}s en ventana ${path.basename(browser)} (visible)`);

  // --user-data-dir propio → instancia dedicada con PID propio (matable).
  const child = spawn(browser, [
    `--app=file:///${htmlPath.replace(/\\/g, '/')}`,
    '--window-size=480,360',
    `--user-data-dir=${path.join(os.tmpdir(), 'sysmon-gpu-stress-profile')}`,
    '--no-first-run', '--no-default-browser-check',
  ], { stdio: 'ignore', windowsHide: false });

  await new Promise((r) => setTimeout(r, durationMs + 3000));
  try { child.kill(); } catch { /* la instancia ya cerró */ }
}

// ---------------------------------------------------------------------------
(async () => {
  log(`escenario: ${scenario}`);
  if (scenario === 'cpu') {
    await stressCpu(35000);
  } else if (scenario === 'ram') {
    await stressRam(25000);
  } else if (scenario === 'gpu') {
    await stressGpu(50000);
  } else if (scenario === 'all') {
    log('lanzando cpu + ram + gpu en paralelo');
    await Promise.all([stressCpu(35000), stressRam(25000), stressGpu(50000)]);
  } else {
    log(`escenario desconocido: '${scenario}'. Usa: cpu | ram | gpu | all`);
    process.exit(1);
  }
  log('carga finalizada — revisa la notificación nativa de Windows');
  process.exit(0);
})();
