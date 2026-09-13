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
 * TEMP (≥80 °C): NO es disparable en esta máquina — el sensor ACPI no está
 * accesible sin elevación (la app muestra 'n/a' + hint), así que no hay dato
 * que llevar por encima del umbral.
 */

const scenario = (process.argv[2] || 'cpu').toLowerCase();
const os = require('node:os');

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
// RAM: reservar ~55% de la memoria total buffers grandes y tocarlos.
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
// GPU: ventana Electron oculta con canvas WebGL corriendo un shader pesado.
// ---------------------------------------------------------------------------
async function stressGpu(durationMs) {
  log(`GPU: estrés WebGL durante ${durationMs / 1000}s (ventana oculta)`);
  const { app, BrowserWindow } = require('electron');
  await app.whenReady();
  const win = new BrowserWindow({
    // COMPLETAMENTE visible (esquina inferior derecha, sin decorar): offscreen
    // recorta el pipeline de GPU y las ventanas casi invisibles las estrangula
    // DWM. Este es el camino de render más real posible.
    width: 480, height: 360, show: true, frame: false,
    x: 8, y: Math.max(0, (require('electron').screen.getPrimaryDisplay().workAreaSize.height ?? 720) - 400),
    webPreferences: { backgroundThrottling: false },
  });
  await win.loadURL('data:text/html,<canvas id="c"></canvas><script>document.getElementById("c").width=2560;document.getElementById("c").height=1440;</script>');
  win.webContents.executeJavaScript(`(() => {
    const c = document.getElementById('c');
    const gl = c.getContext('webgl2') || c.getContext('webgl');
    if (!gl) return 'no-webgl';
    c.width = 2560; c.height = 1440;
    const fs = \`precision highp float; uniform vec2 r; void main() {
      vec2 p = gl_FragCoord.xy / r;
      float v = 0.0;
      for (int i = 0; i < 512; i++) { v += sin(p.x * float(i) + cos(p.y * float(i))); }
      gl_FragColor = vec4(v * 0.002, v * 0.001, abs(sin(v)), 1.0);
    }\`;
    const vs = 'attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }';
    const mk = (t, s) => { const sh = gl.createShader(t); gl.shaderSource(sh, s); gl.compileShader(sh); return sh; };
    const prog = gl.createProgram();
    gl.attachShader(prog, mk(gl.VERTEX_SHADER, vs));
    gl.attachShader(prog, mk(gl.FRAGMENT_SHADER, fs));
    gl.linkProgram(prog); gl.useProgram(prog);
    gl.uniform2f(gl.getUniformLocation(prog, 'r'), 2560, 1440);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1, 3,-1, -1,3]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, 'p');
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    const t0 = Date.now();
    // setInterval (NO requestAnimationFrame): en ventanas ocultas el RAF se
    // limita a ~1 fps y la carga GPU se diluye a nada. Múltiples draws por
    // tick + gl.finish() para que la GPU no encadre trabajo.
    const timer = setInterval(() => {
      if (Date.now() - t0 > ${durationMs}) { clearInterval(timer); return; }
      for (let d = 0; d < 6; d++) gl.drawArrays(gl.TRIANGLES, 0, 3);
      gl.finish();
    }, 12);
    return 'webgl-ok';
  })()`, true).then((r) => log(`GPU: ${r}`));
  return new Promise((r) => setTimeout(r, durationMs + 2000)).then(() => win.destroy());
}

// ---------------------------------------------------------------------------
(async () => {
  // El escenario GPU necesita WebGL = runtime de Electron. Si estamos en Node
  // plano, re-ejecutamos este mismo script a través del binario de Electron.
  if (!process.versions.electron && (scenario === 'gpu' || scenario === 'all')) {
    const { spawn } = require('node:child_process');
    const electronBin = require('electron'); // En Node plano: ruta al binario.
    log('relanzando bajo Electron para WebGL...');
    const child = spawn(electronBin, [__filename, scenario], { stdio: 'inherit', windowsHide: true });
    child.on('exit', (code) => process.exit(code ?? 0));
    return;
  }

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
  log('carga finalizada — revisa el widget: la franja LED debe haberse encendido');
  process.exit(0);
})();
