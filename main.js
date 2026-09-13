'use strict';

/**
 * main.js — Proceso principal de Electron.
 * Widget flotante de monitoreo del sistema con:
 *  - 3 modos de visualización (Mini-Overlay / Dev & Diagnóstico / Gráficas).
 *  - Bandeja (Tray) + atajo global Ctrl+Shift+M para mostrar/ocultar.
 *  - IPC seguro vía ipcMain.handle + caché TTL con single-flight.
 *  - Métricas matemáticamente precisas: CPU por deltas de ticks del kernel,
 *    RAM efectiva (total − available), red diferencial por interfaz activa
 *    y temperatura del paquete CPU.
 */

const { app, BrowserWindow, Tray, Menu, ipcMain, nativeImage, globalShortcut } = require('electron');
const si = require('systeminformation');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { validatePid } = require('./src/lib/validate');

// ---------------------------------------------------------------------------
// Optimización Chromium: desactivar subsistemas que este widget no usa ANTES
// de que la app arranque (deben añadirse antes del evento ready).
// ---------------------------------------------------------------------------
app.commandLine.appendSwitch('disable-speech-api');               // Síntesis de voz: sin uso.
app.commandLine.appendSwitch('disable-renderer-backgrounding');    // No degradar el renderer en 2º plano.
app.commandLine.appendSwitch('disable-background-timer-throttling'); // Timers precisos aunque esté oculto.

/** Impide que Electron se instale como proceso en segundo plano en macOS. */
if (process.platform === 'darwin') app.dock?.hide();

/** Instancia única: un segundo `npm start` enfoca la ventana existente y sale. */
const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (mainWindow) {
      mainWindow.show();
      mainWindow.focus();
    }
  });
}

// ---------------------------------------------------------------------------
// Estado global de la app
// ---------------------------------------------------------------------------
const WIDGET_WIDTH = 340;   // Modos Dev y Gráficas.
const WIDGET_HEIGHT = 440;
const MINI_WIDTH = 340;     // Modo 1: Mini-Overlay compacto para jugar/trabajar.
// 245 px ≈ header (43) + 4 tiras CPU/RAM/TEMP/GPU (~128) + gaps (24) + red (16) + paddings (22) + bordes (2).
// Un colchón extra evita recortes de contenido al re-layoutar.
const MINI_HEIGHT = 245;

/** @type {Electron.BrowserWindow | null} */
let mainWindow = null;
/** @type {Electron.Tray | null} */
let tray = null;
/** Evita que cerrar la ventana con el botón X mate la app (va a la bandeja). */
let appQuitting = false;
/** Modo de visualización vigente ('dev' | 'mini' | 'charts'). */
let widgetMode = 'dev';

// Icono PNG de 16x16 generado en build-time y embebido para evitar
// dependencias de rutas externas.
const TRAY_ICON_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAYAAAAf8/9hAAAAPUlEQVR4nGNgoCp4+v8/UZgizVgNIVUzhiGD2oDX/ykwAKQZhkk2AFkzTkNo6gKqhAEdo5HipEyVzEQmAABHAiCIsCHVkwAAAABJRU5ErkJggg==';

function getTrayIcon() {
  const image = nativeImage.createFromBuffer(Buffer.from(TRAY_ICON_BASE64, 'base64'));
  return image.isEmpty() ? undefined : image;
}

/**
 * Reasenta alwaysOnTop respetando el estado de "fijado" elegido por el usuario.
 * 'screen-saver' es el nivel más alto de Windows: flota incluso sobre juegos.
 * @param {Electron.BrowserWindow} win
 */
function applyPinnedLevel(win) {
  const pinned = Boolean(win.__alwaysOnTopPinned);
  win.setAlwaysOnTop(pinned, pinned ? 'screen-saver' : 'floating');
}

// ---------------------------------------------------------------------------
// Ventana principal
// ---------------------------------------------------------------------------
function createWindow() {
  mainWindow = new BrowserWindow({
    width: WIDGET_WIDTH,
    height: WIDGET_HEIGHT,
    minWidth: MINI_WIDTH,
    minHeight: MINI_HEIGHT,
    frame: false,
    // Ventana SÓLIDA (sin canal alpha): la GPU deja de mantener una superficie
    // D3D compartida para el compositor y usa el camino de render barato
    // (~30-40 MB menos de RAM/GPU). El color replica el --bg de styles.css.
    transparent: false,
    backgroundColor: '#0a0c14',
    alwaysOnTop: true,
    resizable: false,
    hasShadow: false,
    roundedCorners: false,
    show: false,
    icon: getTrayIcon(),
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,   // Aislamiento total de contextos.
      nodeIntegration: false,   // El renderer NO ve Node: solo el puente preload.
      sandbox: false,           // preload necesita require('electron') para el puente.
      backgroundThrottling: false,
      spellcheck: false,        // Sin diccionarios: ahorro de memoria en el renderer.
    },
  });

  mainWindow.__alwaysOnTopPinned = true;
  mainWindow.setMenuBarVisibility(false);
  applyPinnedLevel(mainWindow);
  mainWindow.loadFile(path.join(__dirname, 'src', 'index.html'));

  mainWindow.once('ready-to-show', () => mainWindow?.show());

  // Cerrar la ventana solo la oculta: el widget sigue vivo en la bandeja.
  mainWindow.on('close', (event) => {
    if (!appQuitting) {
      event.preventDefault();
      mainWindow?.hide();
    }
  });

  mainWindow.on('closed', () => {
    mainWindow = null;
  });

  // Reasentar always-on-top cada vez que la ventana recupera el foco.
  mainWindow.on('focus', () => { if (mainWindow) applyPinnedLevel(mainWindow); });
}

// ---------------------------------------------------------------------------
// Modos de visualización (Mini / Dev / Charts)
// ---------------------------------------------------------------------------
/**
 * Cambia el tamaño de la ventana sin tocar resizable:false (setSize funciona
 * independientemente del flag). Ocultar antes de redimensionar evita el
 * parpadeo del frame transparente en Windows al re-layoutar.
 * @param {'dev'|'mini'|'charts'} mode
 */
function setWidgetMode(mode) {
  if (mode !== 'dev' && mode !== 'mini' && mode !== 'charts' && mode !== 'procs') return;
  widgetMode = mode;
  if (!mainWindow) return;

  const width = mode === 'mini' ? MINI_WIDTH : WIDGET_WIDTH;
  const height = mode === 'mini' ? MINI_HEIGHT : WIDGET_HEIGHT;

  mainWindow.setBounds({
    x: mainWindow.getBounds().x,
    y: mainWindow.getBounds().y,
    width,
    height,
  });
}

// ---------------------------------------------------------------------------
// Bandeja del sistema (Tray)
// ---------------------------------------------------------------------------
function createTray() {
  tray = new Tray(getTrayIcon());
  tray.setToolTip('System Monitor Widget — Ctrl+Shift+M');

  // Clic izquierdo: alterna mostrar/ocultar el widget.
  tray.on('click', () => toggleWidget());

  // Menú contextual (clic derecho).
  tray.setContextMenu(
    Menu.buildFromTemplate([
      {
        label: 'Show / Hide  (Ctrl+Shift+M)',
        click: () => toggleWidget(),
      },
      { type: 'separator' },
      { label: 'Quit', click: () => quitApp() },
    ])
  );
}

function toggleWidget() {
  if (!mainWindow) return;
  if (mainWindow.isVisible()) {
    mainWindow.hide();
  } else {
    mainWindow.show();
    mainWindow.focus();
  }
}

function quitApp() {
  appQuitting = true;
  app.quit();
}

// ---------------------------------------------------------------------------
// Utilidades
// ---------------------------------------------------------------------------
// Utilidades PURAS compartidas con el renderer y los tests: viven en un módulo
// UMD (src/lib/metrics.js) para poder probarse en Node sin arrancar Electron.
const { toNumber, round1, round2, pickActiveIface, parseGpuCsvLine } = require('./src/lib/metrics');

/** Evita que un PID malicioso derrame bytes nulos en el nombre del proceso. */
const sanitize = (value) => String(value ?? '').replace(/[\0\r\n]+/g, ' ').trim();

// ---------------------------------------------------------------------------
// Caché TTL con single-flight: si el renderer llama al mismo canal varias
// veces dentro de la ventana TTL, se sirve la última medición en vez de
// repetir lecturas caras (si.processes() enumera TODOS los procesos del SO).
// ---------------------------------------------------------------------------
// Las lecturas de red/procesos en Windows pasan por WMI y en muchas máquinas
// tardan varios segundos por query: el polling del renderer SIEMPRE se sirve
// desde esta caché y solo un query real puede estar en vuelo a la vez.
const STATS_TTL_MS = 2500;  // Igual al polling del renderer (2.5 s).
const PROC_TTL_MS = 10000;  // Enumerar procesos cuesta ~7 s (WMI): 1 query real / 10 s.

// Caché TTL + single-flight extraída a un módulo para poder testearla en Node
// sin arrancar Electron (test/main.test.js).
const { cached, invalidate } = require('./src/lib/cache');

/**
 * Warm-up de CPU: si.currentLoad() calcula la carga comparando DOS muestras.
 * La primera lectura de la sesión puede venir inflada por el arranque (picos
 * falsos de hasta 100%). Calentamos la medición en segundo plano y el handler
 * espera esta promesa, de modo que el widget nunca muestre un pico artificial.
 */
const cpuWarmup = (async () => {
  await si.currentLoad(); // Muestra de referencia (se descarta).
  await new Promise((r) => setTimeout(r, 350));
  await si.currentLoad();
})().catch(() => {}); // Nunca rechaza: si falla, el handler reintenta normal.

// ---------------------------------------------------------------------------
// Temperatura del CPU: lectura DESACOPLADA del camino crítico.
// En muchas máquinas Windows, si.cpuTemperature() consulta WMI
// (MSAcpi_ThermalZoneTemperature) y ese query tarda ~30 s en expirar cuando el
// sensor no está expuesto. Medir en línea dentro de get-system-stats
// congelaría el widget hasta 36 s por ciclo: se refresca en segundo plano y
// el handler sirve el último valor conocido al instante.
// ---------------------------------------------------------------------------
const TEMP_VALID_MIN = 10;        // Ruido bajo este valor: sensores que devuelven 0.
const TEMP_INVALID = -1;          // Sentinel de cpuTempCache: sensor no disponible.
const CPU_TEMP_REFRESH_MS = 60_000; // Reintento en segundo plano: 1 query WMI/min.

let cpuTempCache = -1;            // -1: sin medir aún / sensor no disponible.
let cpuTempInflight = false;      // Evita solapar queries WMI de 30 s.

async function refreshCpuTemp() {
  if (cpuTempInflight) return;
  cpuTempInflight = true;
  try {
    // Con elevación, la lectura ACPI directa es más fiable que el parseo
    // interno de SI (mismo costo WMI, menos capas).
    if (isAdmin) {
      const direct = await readAcpiTempDirect();
      if (direct !== null) {
        cpuTempCache = direct;
        return;
      }
      // null: zonas sin dato → cae al camino SI por si otro sensor existe.
    }
    const temp = await si.cpuTemperature();
    const main = toNumber(temp?.main);
    if (main >= TEMP_VALID_MIN) {
      cpuTempCache = round1(main);
    } else {
      const cores = Array.isArray(temp?.cores) ? temp.cores : [];
      const maxCore = cores.reduce((acc, c) => Math.max(acc, toNumber(c)), 0);
      if (maxCore >= TEMP_VALID_MIN) cpuTempCache = round1(maxCore);
      // Sin sensor real: se conserva el último valor (-1 → 'n/a' en la UI).
    }
  } catch {
    /* sensor ausente: conserva el último valor */
  } finally {
    cpuTempInflight = false;
  }
}

/** Cadena auto-programada: nunca hay dos queries WMI en paralelo. */
function scheduleCpuTempRefresh() {
  Promise.resolve(refreshCpuTemp())
    .catch(() => {})
    .then(() => setTimeout(scheduleCpuTempRefresh, CPU_TEMP_REFRESH_MS));
}

// ---------------------------------------------------------------------------
// Permisos: en Windows, la clase ACPI MSAcpi_ThermalZoneTemperature exige
// ELEVACIÓN (probado en esta máquina: "Acceso denegado" sin admin). si.
// cpuTemperature() degradará siempre ahí; con elevación sí devuelve datos.
// ---------------------------------------------------------------------------
const isAdmin = process.platform === 'win32'
  ? (() => {
      try {
        // Chequeo por TOKEN (IsInRole) en lugar de `net session`: este último
        // también falla cuando el servicio "Server" (LanmanServer) está detenido,
        // dando un falso "no admin" AUN con elevación.
        const out = require('node:child_process')
          .execSync(
            'powershell -NoProfile -Command "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)"',
            { stdio: ['ignore', 'pipe', 'ignore'], windowsHide: true, timeout: 8000 }
          )
          .toString()
          .trim();
        return out === 'True';
      } catch {
        return false;
      }
    })()
  : process.getuid?.() === 0;

/**
 * Lectura ACPI DIRECTA (solo con elevación): decodifica Kelvin×10 → °C.
 * Devuelve null si la clase sigue sin exponer zonas (hardware sin sensor).
 */
async function readAcpiTempDirect() {
  try {
    const { exec } = require('node:child_process');
    const { stdout } = await new Promise((resolve, reject) => {
      exec(
        'powershell -NoProfile -Command "(Get-CimInstance -Namespace root/wmi -ClassName MSAcpi_ThermalZoneTemperature | Select-Object -First 1).CurrentTemperature"',
        { windowsHide: true, timeout: 15_000 },
        (err, so) => (err ? reject(err) : resolve({ stdout: String(so ?? '') }))
      );
    });
    const kelvinX10 = Number(stdout.trim());
    if (!Number.isFinite(kelvinX10) || kelvinX10 <= 0) return null;
    const celsius = kelvinX10 / 10 - 273.15;
    // Rango físico plausible: los sensores ACPI no reportan <0 ni >120 °C.
    return celsius >= 0 && celsius <= 120 ? Math.round(celsius * 10) / 10 : null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// GPU: uso real vía contadores de rendimiento de Windows (PDH), la misma
// fuente que el Administrador de Tareas. si.graphics() NO reporta
// utilizationGpu en iGPUs Intel y la clase WMI GpuEnginePerformance no está
// expuesta a CIM en muchas máquinas. `typeperf` es un binario nativo ligero:
// UN solo proceso hijo PERSISTENTE emite CSV a stdout cada 2 s — cero WMI,
// cero PowerShell, cero procesos creados por ciclo de muestreo.
// ---------------------------------------------------------------------------
const GPU_MAX_RESTARTS = 3;       // Tope de reinicios antes de declarar 'no disponible'.
const GPU_RESTART_DELAY_MS = 10_000;

let gpuUtilCache = -1;            // -1: sin dato aún (arranque o contadores ausentes).
let gpuCountersAvailable = true;  // false: la clase de contadores no existe en este SO.
let gpuChild = null;
let gpuRestarts = 0;
let gpuRestartTimer = null;


function stopGpuSampler() {
  if (gpuRestartTimer) { clearTimeout(gpuRestartTimer); gpuRestartTimer = null; }
  if (gpuChild) { try { gpuChild.kill(); } catch { /* ya muerto */ } gpuChild = null; }
}

function startGpuSampler() {
  if (process.platform !== 'win32' || !gpuCountersAvailable) return;
  gpuChild = spawn(
    'typeperf',
    ['\\GPU Engine(*)\\Utilization Percentage', '-si', '2'],
    { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] }
  );

  let buffer = '';
  let columnTypes = null; // engtype por columna, extraído de la cabecera PDH-CSV.
  gpuChild.stdout.setEncoding('utf8');
  gpuChild.stdout.on('data', (chunk) => {
    buffer += chunk;
    let nl;
    while ((nl = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, nl).replace(/\r$/, '');
      buffer = buffer.slice(nl + 1);
      // Cabecera: "\\HOST\GPU Engine(pid_..._engtype_3D)\Utilization Percentage",...
      if (line.startsWith('"(PDH-CSV')) {
        const headerCols = [];
        let hCur = '';
        let hInQ = false;
        for (let i = 0; i < line.length; i++) {
          const ch = line[i];
          if (ch === '"') { hInQ = !hInQ; continue; }
          if (ch === ',' && !hInQ) { headerCols.push(hCur); hCur = ''; continue; }
          hCur += ch;
        }
        headerCols.push(hCur);
        columnTypes = headerCols.slice(1).map((h) => {
          const m = /engtype_([^)]+)\)/i.exec(h);
          return m ? m[1].trim().toLowerCase() : 'unknown';
        });
        continue;
      }
      const value = parseGpuCsvLine(line, columnTypes);
      if (value !== null) gpuUtilCache = Math.round(value * 10) / 10;
    }
  });

  gpuChild.on('error', () => {
    // typeperf inexistente o no ejecutable: no hay vía de contadores.
    gpuCountersAvailable = false;
    gpuChild = null;
  });

  gpuChild.on('exit', (code) => {
    gpuChild = null;
    if (!gpuCountersAvailable || appQuitting) return;
    if (code === 0) { gpuCountersAvailable = false; return; } // clase inexistente
    if (gpuRestarts >= GPU_MAX_RESTARTS) { gpuCountersAvailable = false; return; }
    gpuRestarts += 1;
    gpuRestartTimer = setTimeout(startGpuSampler, GPU_RESTART_DELAY_MS);
  });
}

// ---------------------------------------------------------------------------
// Manejadores IPC
// ---------------------------------------------------------------------------

/**
 * Red: muestrear SOLO la interfaz de la ruta por defecto.
 * networkStats('*') lanza queries WMI por CADA adaptador (~15 en un equipo
 * típico) y en máquinas con WMI lento cuesta 2-8 s por ciclo. La interfaz de
 * la ruta por defecto ES la activa real y, por construcción, nunca es un
 * adaptador virtual (Docker/WSL/VPN no llevan la ruta por defecto).
 * pickActiveIface() se conserva como filtro de seguridad sobre el resultado.
 */
const NET_IFACE_TTL_MS = 30_000; // Re-resolver la ruta por defecto 1 vez / 30 s.
let netDefaultIface = { at: 0, promise: null };

function getDefaultIface() {
  const now = Date.now();
  if (netDefaultIface.promise && now - netDefaultIface.at < NET_IFACE_TTL_MS) {
    return netDefaultIface.promise;
  }
  netDefaultIface = {
    at: now,
    promise: si.networkInterfaceDefault().catch(() => ''),
  };
  return netDefaultIface.promise;
}

/**
 * Caché single-flight de la lectura de red. La query de arranque (prewarm) se
 * REUTILIZA como primera lectura real: no hay queries duplicadas ni contención
 * entre el warm-up y el primer poll del renderer.
 */
let netData = [];        // Última muestra resuelta (stale ≤ 1 ciclo si hay demora).
let netPending = null;   // Query en vuelo (single-flight: jamás 2 en paralelo).

/** Resuelve con fallback tras ms sin esperar la promesa (0 = fallback inmediato). */
function withTimeout(promise, ms, fallback) {
  if (!ms) return Promise.resolve(fallback);
  return Promise.race([promise, new Promise((r) => setTimeout(() => r(fallback), ms))]);
}

/**
 * Lanza (o reutiliza) la query de red. Nunca bloquea el ciclo más de
 * NET_BUDGET_MS: si WMI demora, el handler sirve la muestra anterior y la
 * query en curso alimenta el siguiente ciclo (self-healing).
 */
const NET_BUDGET_MS = 400;

function getNetStats() {
  if (!netPending) {
    netPending = (async () => {
      // 1 query WMI (interfaz por defecto) en vez de 1 por adaptador.
      // Fallback a '*' solo si la resolución de la ruta falla.
      const defIface = await getDefaultIface();
      const data = defIface ? await si.networkStats(defIface) : await si.networkStats('*');
      netData = Array.isArray(data) ? data : [];
      netPending = null;
      return netData;
    })().catch(() => {
      netPending = null;
      return netData;
    });
  }
  return netPending;
}


/**
 * get-system-stats: CPU (%), RAM (% y GB), red (B/s crudos) y temperatura °C.
 * Lecturas reales como máximo cada STATS_TTL_MS aunque se invoque más seguido.
 */
ipcMain.handle('get-system-stats', () =>
  cached('stats', STATS_TTL_MS, async () => {
    try {
      // Red: la query se lanza YA, en paralelo con el warm-up de CPU.
      const netQuery = getNetStats();
      // GPU: lectura instantánea desde el sampler persistente (typeperf).
      const gpuUtil = gpuUtilCache;

      // La 1ª lectura de la sesión espera al warm-up (~600 ms una sola vez).
      await cpuWarmup;
      const [cpuLoad, memory] = await Promise.all([si.currentLoad(), si.mem()]);

      // Presupuesto de espera: con muestra previa 400 ms (sirve stale si WMI
      // se demora); sin muestra previa (arranque) 0 ms → 'n/a' este ciclo y
      // dato real desde el siguiente. El ciclo NUNCA espera a WMI.
      const hadNet = netData.length > 0;
      const net = await withTimeout(netQuery, hadNet ? NET_BUDGET_MS : 0, netData);
      const iface = pickActiveIface(net);

      // RAM REAL EN USO: total - available (NUNCA total - free).
      // `available` es la memoria que el SO puede entregar a procesos sin
      // swapping: contempla buffers/cachés recuperables en Windows/Linux/macOS.
      const usedBytes = Math.max(toNumber(memory.total) - toNumber(memory.available), 0);

      // Estado del sensor térmico para la UI (honesto en los 3 casos):
      //   'ok'    → lectura válida en cpuTemp.
      //   'admin' → sin datos Y sin elevación: ambiguo (¿permisos o sin
      //             sensor? el WMI no lo distingue sin elevar) → sugerencia.
      //   'none'  → sin datos AUN elevado (u otro SO): el hardware no
      //             publica la zona térmica → n/a definitivo, sin hint falso.
      let tempStatus = 'ok';
      if (cpuTempCache <= TEMP_INVALID) {
        tempStatus = process.platform === 'win32' && !isAdmin ? 'admin' : 'none';
      }

      return {
        ok: true,
        // CPU: uso global real instantáneo (deltas de ticks del kernel), 1 decimal.
        cpu: round1(cpuLoad.currentLoad),
        memory: {
          percent: round1((usedBytes / toNumber(memory.total)) * 100),
          usedGb: round2(usedBytes / 1024 ** 3),
          totalGb: round2(toNumber(memory.total) / 1024 ** 3),
        },
        network: {
          iface: String(iface.iface ?? 'n/a'),
          // Bytes/s crudos (negativos = primer sample de SI → 0): el renderer
          // aplica la conversión dinámica KB/s ↔ MB/s.
          rxBytesSec: round1(Math.max(toNumber(iface.rx_sec), 0)),
          txBytesSec: round1(Math.max(toNumber(iface.tx_sec), 0)),
        },
        // Temperatura del paquete CPU en °C (-1 = sensor no disponible).
        // Sirve el último valor del refresco en segundo plano: lectura
        // instantánea, jamás bloquea el ciclo de muestreo con WMI.
        cpuTemp: cpuTempCache,
        tempStatus,
        // Uso real de GPU en % (contadores PDH por motor; -1 = sin dato).
        gpu: gpuUtil,
      };
    } catch (error) {
      return { ok: false, error: error instanceof Error ? error.message : String(error) };
    }
  })
);

/**
 * get-top-processes: Top 5 procesos ordenados por uso de CPU.
 * Omite procesos inactivos de sistema ("System Idle Process", "System Interrupts").
 * Retorna [{ pid, name, cpu, mem }]
 */
ipcMain.handle('get-top-processes', () =>
  cached('procs', PROC_TTL_MS, async () => {
    try {
      const { list } = await si.processes();
      // Optimización: selección parcial (top-5 por CPU) SIN ordenar los ~225
      // procesos completos. Filtrado + una pasada de selección O(n·k), k=5.
      const NOISE = /^(system idle process|system interrupts)$/i;
      const top = [];
      for (const p of list) {
        const pid = Number(p.pid);
        const name = sanitize(p.name);
        if (!(pid > 0) || NOISE.test(name)) continue;
        const item = { pid, name, cpu: round1(p.cpu), mem: round2(p.mem) };
        if (top.length < 5) {
          top.push(item);
          if (top.length === 5) top.sort((a, b) => b.cpu - a.cpu);
        } else if (item.cpu > top[4].cpu) {
          top[4] = item;
          top.sort((a, b) => b.cpu - a.cpu);
        }
      }
      return { ok: true, processes: top };
    } catch (error) {
      return { ok: false, error: error instanceof Error ? error.message : String(error) };
    }
  })
);

/**
 * get-gpu-info: metadatos de la GPU (nombre, driver, VRAM) vía si.graphics().
 * Consulta lenta (~2 s): se cachea con TTL + single-flight para no repetir el
 * query si el renderer llama varias veces seguidas, pero refresca cada
 * GPU_INFO_TTL_MS por si el hardware/driver cambia.
 */
const GPU_INFO_TTL_MS = 10 * 60 * 1000; // Refrescar 1 vez cada 10 min.
let gpuInfoCache = null;
let gpuInfoAt = 0;
let gpuInfoInflight = null;

function fetchGpuInfo() {
  if (gpuInfoInflight) return gpuInfoInflight;
  gpuInfoInflight = si.graphics()
    .then((g) => {
      const c = (g?.controllers ?? []).find((k) => k.model) ?? null;
      return {
        ok: true,
        model: c ? String(c.model) : 'GPU',
        vendor: c?.vendor ? String(c.vendor) : '',
        vramMb: Number.isFinite(Number(c?.vram)) ? Number(c.vram) : null,
        driver: c?.driverVersion ? String(c.driverVersion) : '',
      };
    })
    .catch(() => ({ ok: false, model: 'GPU', vendor: '', vramMb: null, driver: '' }))
    .then((result) => {
      gpuInfoCache = result;
      gpuInfoAt = Date.now();
      return result;
    })
    .finally(() => { gpuInfoInflight = null; });
  return gpuInfoInflight;
}

ipcMain.handle('get-gpu-info', () => {
  if (gpuInfoCache && Date.now() - gpuInfoAt < GPU_INFO_TTL_MS) return gpuInfoCache;
  return fetchGpuInfo();
});

/**
 * kill-process: Termina un proceso por PID de forma segura con control de
 * excepciones (EPERM: sin permisos; ESRCH: ya no existe).
 * - Windows: `taskkill /F /T` (árboles de proceso; process.kill no los corta).
 * - POSIX:   `process.kill(pid)` (SIGTERM por defecto).
 */
ipcMain.handle('kill-process', async (_event, pid) => {
  // Validación extraída a un módulo (testeable sin Electron).
  const check = validatePid(pid, process.pid);
  if (!check.ok) return check;

  const numericPid = check.pid;

  try {
    if (process.platform === 'win32') {
      const { execFile } = require('node:child_process');
      await new Promise((resolve, reject) => {
        execFile('taskkill', ['/F', '/T', '/PID', String(numericPid)], { windowsHide: true }, (err) =>
          err ? reject(err) : resolve()
        );
      });
    } else {
      process.kill(numericPid); // SIGTERM por defecto.
    }
    // Invalida la caché de procesos: el PID muerto desaparece en el próximo refresh.
    invalidate('procs');
    return { ok: true, pid: numericPid };
  } catch (error) {
    const code = error && typeof error === 'object' && 'code' in error ? error.code : undefined;
    const msg =
      code === 'ESRCH'
        ? `Process ${numericPid} no longer exists`
        : `Failed to kill process ${numericPid}${code ? ` (${code})` : ''}`;
    return { ok: false, error: msg };
  }
});

/** toggle-always-on-top: fija / desfija el widget sobre otras apps. */
ipcMain.handle('toggle-always-on-top', () => {
  if (!mainWindow) return { ok: false, error: 'No window' };
  mainWindow.__alwaysOnTopPinned = !mainWindow.__alwaysOnTopPinned;
  applyPinnedLevel(mainWindow);
  return { ok: true, pinned: Boolean(mainWindow.__alwaysOnTopPinned) };
});

/** get-always-on-top: consulta el estado actual (para pintar el botón al arrancar). */
ipcMain.handle('get-always-on-top', () => ({
  ok: true,
  pinned: Boolean(mainWindow?.__alwaysOnTopPinned),
}));

/** set-widget-mode: redimensiona la ventana para Mini / Dev / Charts / Procs. */
ipcMain.handle('set-widget-mode', (_event, mode) => {
  if (typeof mode !== 'string') return { ok: false, error: 'Invalid mode' };
  setWidgetMode(mode);
  return { ok: true, mode: widgetMode };
});

// ---------------------------------------------------------------------------
// Ciclo de vida de la app
// ---------------------------------------------------------------------------
app.whenReady().then(() => {
  // Identidad de app para notificaciones nativas de Windows: sin AppUserModelID
  // los toasts del renderer salen bajo la identidad genérica de "Electron" o
  // fallan en el build empaquetado.
  if (process.platform === 'win32') app.setAppUserModelId('com.breiner.sysmonwidget');

  createWindow();
  createTray();

  // Pre-calentamiento WMI: la 1ª query de cada clase inicializa COM (~3-5 s
  // en máquinas lentas). Esta query SE REGISTRA en netPending/netData: el
  // primer poll del renderer la reutiliza (single-flight) o sirve 'n/a'.
  getNetStats();
  // El enumerado de procesos es lo más pesado: se difiere para no robar
  // CPU/disco al arranque y no retrasar la primera pintura del widget.
  setTimeout(() => { si.processes().catch(() => {}); }, 2500);

  // Refresco de temperatura en segundo plano (WMI lento: nunca en línea).
  scheduleCpuTempRefresh();

  // Sampler GPU: proceso typeperf persistente (una sola vez, no por ciclo).
  startGpuSampler();

  // Atajo global nativo: Ctrl+Shift+M alterna el widget al instante,
  // esté o no enfocada la aplicación.
  globalShortcut.register('Ctrl+Shift+M', () => toggleWidget());

  app.on('activate', () => {
    // En macOS re-crear la ventana si el dock es reactivado.
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
    else mainWindow?.show();
  });
});

/** Libera el atajo global al salir: si otra app lo pide, debe poder tomarlo. */
app.on('before-quit', () => {
  appQuitting = true;
  stopGpuSampler();
  globalShortcut.unregister('Ctrl+Shift+M');
  globalShortcut.unregisterAll();
});

// Comportamiento tipo widget: cerrar todas las ventanas NO apaga la app (queda en bandeja).
app.on('window-all-closed', (event) => {
  // Intencional: no llamar a app.quit(); el widget vive en la bandeja.
});
