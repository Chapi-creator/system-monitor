'use strict';

/**
 * renderer.js — Consumo IPC y renderizado de nivel producción.
 * - Un único intervalo de muestreo a 2500 ms (ciclo de bajo consumo).
 * - Chart.js con animaciones desactivadas, sin puntos, update('none') e
 *   historial estricto de 20 puntos por serie (CPU/TEMP/RAM/GPU/NET).
 * - Huecos honestos: TEMP/GPU sin dato se guardan como null → la línea de la
 *   gráfica se INTERRUMPE (spanGaps:false) en vez de inventar valores.
 * - Trabajo por modo: procesos SOLO en modo Dev, canvas SOLO en modo Gráficas.
 * - Guardián proactivo: ≥85% CPU o ≥80 °C durante 2 lecturas consecutivas
 *   (5 s) → Notification() nativa HTML5 con anti-flood.
 * - DOM: filas y nodos creados una vez y reutilizados; solo textContent/width.
 */

/* global Chart, SysMonMetrics */

// Utilidades puras compartidas (módulo UMD cargado antes que este script).
const { formatSpeed } = window.SysMonMetrics;

const METRICS_INTERVAL_MS = 2500;  // Especificación: 2.5 s por ciclo.
const MAX_POINTS = 20;             // Límite estricto del historial de gráficas.

// Umbrales del guardián proactivo: TODAS las métricas cubiertas.
// Centralizados en un solo objeto para no repetir valores hardcodeados en el
// guardián, el renderizado Dev y el mini overlay. Editar aquí afecta a todos.
const THRESHOLDS = Object.freeze({
  cpu: 85,   // % de CPU sostenida.
  temp: 80,  // °C del paquete CPU.
  gpu: 90,   // % de GPU sostenida.
  ram: 90,   // % de RAM sostenida.
});
const ALERT_STREAK_FOR_NOTIFICATION = 2; // 2 lecturas consecutivas = ~5 s.
const NOTIFY_COOLDOWN_MS = 60_000; // Anti-flood entre notificaciones.
const TEMP_INVALID = -1;           // Sentinel: sensor no disponible.
const GPU_INVALID = -1;            // Sentinel: contadores GPU no disponibles.

const el = {
  // Header
  btnPin: document.getElementById('btn-pin'),
  btnMini: document.getElementById('btn-mini'),
  btnDev: document.getElementById('btn-dev'),
  btnCharts: document.getElementById('btn-charts'),
  btnProcs: document.getElementById('btn-procs'),
  btnClose: document.getElementById('btn-close'),
  // Modo mini
  modeMini: document.getElementById('mode-mini'),
  miniCpuBar: document.getElementById('mini-cpu-bar'),
  miniCpuValue: document.getElementById('mini-cpu-value'),
  miniRamBar: document.getElementById('mini-ram-bar'),
  miniRamValue: document.getElementById('mini-ram-value'),
  miniTempBar: document.getElementById('mini-temp-bar'),
  miniTempValue: document.getElementById('mini-temp-value'),
  miniGpuBar: document.getElementById('mini-gpu-bar'),
  miniGpuValue: document.getElementById('mini-gpu-value'),
  miniCpu: document.getElementById('mini-cpu'),
  miniRam: document.getElementById('mini-ram'),
  miniNetIface: document.getElementById('mini-net-iface'),
  miniNetDown: document.getElementById('mini-net-down'),
  miniNetUp: document.getElementById('mini-net-up'),
  // Cuerpo
  widgetBody: document.querySelector('.widget__body'),
  tabMonitor: document.getElementById('tab-monitor'),
  tabCharts: document.getElementById('tab-charts'),
  cpuValue: document.getElementById('cpu-value'),
  cpuBar: document.getElementById('cpu-bar'),
  cpuCard: document.getElementById('card-cpu'),
  tempValue: document.getElementById('temp-value'),
  tempBar: document.getElementById('temp-bar'),
  tempCard: document.getElementById('card-temp'),
  tempAdminHint: document.getElementById('temp-admin-hint'),
  tempNoneHint: document.getElementById('temp-none-hint'),
  ramCard: document.getElementById('card-ram'),
  gpuValue: document.getElementById('gpu-value'),
  gpuBar: document.getElementById('gpu-bar'),
  gpuCard: document.getElementById('card-gpu'),
  gpuDetail: document.getElementById('gpu-detail'),
  ramValue: document.getElementById('ram-value'),
  ramBar: document.getElementById('ram-bar'),
  ramDetail: document.getElementById('ram-detail'),
  netIface: document.getElementById('net-iface'),
  netDown: document.getElementById('net-down'),
  netUp: document.getElementById('net-up'),
  // Modo Procesos (lista dedicada a pantalla completa)
  tabProcs: document.getElementById('tab-procs'),
  processListFull: document.getElementById('process-list-full'),
  listEmptyFull: document.getElementById('list-empty-full'),
  listFootTextFull: document.getElementById('list-foot-text-full'),
  btnRefreshProcs: document.getElementById('btn-refresh-procs'),
  // Gráficas
  cCpu: document.getElementById('c-cpu'),
  cTemp: document.getElementById('c-temp'),
  cRam: document.getElementById('c-ram'),
  cGpu: document.getElementById('c-gpu'),
  cNet: document.getElementById('c-net'),
  chartCpu: document.getElementById('chart-cpu'),
  chartTemp: document.getElementById('chart-temp'),
  chartRam: document.getElementById('chart-ram'),
  chartGpu: document.getElementById('chart-gpu'),
  chartNet: document.getElementById('chart-net'),
  widget: document.getElementById('widget'),
};

// ---------------------------------------------------------------------------
// Utilidades de formato
// ---------------------------------------------------------------------------
const fmt1 = (n) => (Number.isFinite(n) ? n.toFixed(1) : '--');
const clampPct = (n) => Math.min(100, Math.max(0, Number.isFinite(n) ? n : 0));


/** Temperatura legible: '-1' (sentinel) → 'n/a'. */
function formatTemp(celsius) {
  const t = Number(celsius);
  return Number.isFinite(t) && t > 0 ? t.toFixed(0) : 'n/a';
}

/** GPU legible: '-1' (sentinel) → 'n/a' (contadores no disponibles aún). */
function formatGpu(pct) {
  const v = Number(pct);
  return Number.isFinite(v) && v > GPU_INVALID ? v.toFixed(0) : 'n/a';
}

// ---------------------------------------------------------------------------
// Gestión de modos: 'dev' | 'mini' | 'charts'
// ---------------------------------------------------------------------------
let activeMode = 'dev';
let lastStats = null;

function applyModeUI(mode) {
  const isMini = mode === 'mini';
  const isCharts = mode === 'charts';
  const isProcs = mode === 'procs';

  // data-mode gobierna el CSS (.widget[data-mode="mini"] → height auto).
  el.widget.dataset.mode = mode;
  el.modeMini.hidden = !isMini;
  el.widgetBody.hidden = isMini;

  el.tabMonitor.hidden = isCharts || isMini || isProcs;
  el.tabCharts.hidden = !isCharts;
  el.tabProcs.hidden = !isProcs;

  el.btnMini.classList.toggle('icon-btn--active', isMini);
  el.btnDev.classList.toggle('icon-btn--active', mode === 'dev');
  el.btnCharts.classList.toggle('icon-btn--active', isCharts);
  el.btnProcs.classList.toggle('icon-btn--active', isProcs);
}

async function setMode(mode, fromBackend = false) {
  if (mode === activeMode) return;
  activeMode = mode;
  applyModeUI(mode);
  // En la ruta backend-driven NO re-invocamos el comando: Rust ya fijó el modo
  // (y su dedup evita re-emisiones). Re-invocar aquí causaba un eco
  // renderer→backend→renderer que duplicaba cada evento 'mode-changed'.
  if (!fromBackend) window.api.setWidgetMode(mode);

  if (mode === 'charts') {
    ensureCharts();
    if (lastStats) updateCharts(lastStats);
  }
  if (mode === 'mini' && lastStats) {
    renderMini(lastStats); // Pintado inmediato: sin esperar el próximo ciclo.
  }
  if ((mode === 'dev' || mode === 'procs') && lastStats) {
    if (mode === 'dev') renderDevPanel(lastStats);
    fetchProcesses(true); // La lista pudo quedar desactualizada en otros modos.
  }
}

// ---------------------------------------------------------------------------
// Chart.js: configuración de extrema optimización
// ---------------------------------------------------------------------------
/** Historial con tope estricto de 20 muestras por serie (null = sin dato). */
const history = { cpu: [], temp: [], ram: [], gpu: [], net: [] };

/** Techo de la escala Y del chart GPU: nunca por debajo de 10%. */
const GPU_Y_FLOOR = 10;

function pushCapped(arr, value) {
  arr.push(value);
  if (arr.length > MAX_POINTS) arr.shift(); // shift() al superar los 20 puntos.
}

const charts = { cpu: null, temp: null, ram: null, gpu: null, net: null };

/** Base común: cero animaciones, cero puntos, cero tooltips, cero leyenda. */
function baseDataset(label, color, fill) {
  return {
    label,
    data: [],
    borderColor: color,
    backgroundColor: fill,
    borderWidth: 1.5,
    fill: true,
    tension: 0.3,
    pointRadius: 0,          // Sin puntos de trazado.
    pointHitRadius: 0,
    spanGaps: false,         // null interrumpe la línea: hueco honesto, no cero falso.
  };
}

function baseScales() {
  return {
    x: {
      display: false,
      type: 'category',
      labels: Array.from({ length: MAX_POINTS }, (_, i) => String(i)),
    },
    y: {
      display: false,
      beginAtZero: true,
      suggestedMax: 100,
    },
  };
}

function makeChart(canvas, label, color, fill, yMax) {
  const scales = baseScales();
  scales.y.suggestedMax = yMax;
  return new Chart(canvas, {
    type: 'line',
    data: { datasets: [baseDataset(label, color, fill)] },
    options: {
      animation: false,          // Sin animación: 'none' implícito en cada update.
      responsive: true,
      maintainAspectRatio: false,
      events: [],                // Sin listeners de hover/click.
      plugins: {
        legend: { display: false },
        tooltip: { enabled: false },
      },
      elements: { point: { radius: 0 } },
      scales,
    },
  });
}

function ensureCharts() {
  if (charts.cpu) return; // Ya inicializados (lazy: solo al entrar al modo).
  charts.cpu = makeChart(el.chartCpu, 'CPU', '#00ffe5', 'rgba(0,255,229,0.12)', 100);
  charts.temp = makeChart(el.chartTemp, 'TEMP', '#ffb020', 'rgba(255,176,32,0.12)', 100);
  charts.ram = makeChart(el.chartRam, 'RAM', '#ff2bd6', 'rgba(255,43,214,0.12)', 100);
  charts.gpu = makeChart(el.chartGpu, 'GPU', '#9d6bff', 'rgba(157,107,255,0.12)', 100);
  // Red: escala Y dinámica (suggestedMax = pico reciente, mínimo 64 KiB/s).
  charts.net = makeChart(el.chartNet, 'NET', '#ffb020', 'rgba(255,176,32,0.12)', 64);
  for (const key of Object.keys(history)) {
    charts[key].data.datasets[0].data = history[key]; // Referencia viva al historial.
  }
}

function updateCharts(stats) {
  const netKib = (Number(stats.network?.rxBytesSec) + Number(stats.network?.txBytesSec)) / 1024;
  const netChart = charts.net;
  if (netChart) {
    const peak = Math.max(...history.net.filter((v) => v !== null), 64);
    netChart.options.scales.y.suggestedMax = peak; // Auto-escala barata.
  }
  // GPU idle queda invisible a escala 0-100: la Y se ajusta al pico reciente
  // (mínimo 10%) para que 2-9% de uso real sea una línea legible.
  const gpuChart = charts.gpu;
  if (gpuChart) {
    const peak = Math.max(...history.gpu.filter((v) => v !== null), GPU_Y_FLOOR);
    gpuChart.options.scales.y.suggestedMax = Math.ceil(peak * 1.2);
  }

  // update('none'): sin animación, sin re-layout extra.
  for (const key of Object.keys(charts)) charts[key]?.update('none');

  el.cCpu.textContent = fmt1(clampPct(stats.cpu));
  el.cTemp.textContent = formatTemp(stats.cpuTemp);
  el.cRam.textContent = fmt1(clampPct(stats.memory?.percent));
  el.cGpu.textContent = formatGpu(stats.gpu);
  el.cNet.textContent = netKib.toFixed(1);
}

// ---------------------------------------------------------------------------
// Guardián proactivo: alertas en segundo plano
// ---------------------------------------------------------------------------
const guardian = {
  cpuStreak: 0,
  tempStreak: 0,
  gpuStreak: 0,
  ramStreak: 0,
  //Latch por tipo: la notificación sale UNA vez por episodio (al alcanzar el
  // streak mínimo) aunque un ciclo se salte o se retrase: con comparación
  // exacta (streak === N) un tick perdido se comía la alerta del episodio.
  cpuFired: false,
  tempFired: false,
  gpuFired: false,
  ramFired: false,
  lastNotifyAt: {}, // Cooldown POR tipo: una alerta de CPU no silencia una de RAM.
};

/**
 * Emite una Notification() nativa HTML5 con cooldown anti-flood de 60 s POR
 * TIPO de alerta (alertKey): CPU y RAM sobre umbral a la vez = 2 notificaciones.
 * @param {string} alertKey 'cpu' | 'temp' | 'gpu' | 'ram'
 * @param {string} title
 * @param {string} body
 */
function notify(alertKey, title, body) {
  const now = Date.now();
  if (now - (guardian.lastNotifyAt[alertKey] ?? 0) < NOTIFY_COOLDOWN_MS) return;
  guardian.lastNotifyAt[alertKey] = now;
  try {
    const notification = new Notification(title, { body, silent: false });
    notification.onclick = () => notification.close();
  } catch {
    /* Entornos sin permiso de notificación: la alerta simplemente se omite. */
  }
}

/**
 * Evalúa CPU y temperatura contra los umbrales del guardián.
 * Solo notifica con N lecturas consecutivas sobre umbral (~5 s a 2.5 s/ciclo).
 */
function runGuardian(stats) {
  const cpu = clampPct(stats.cpu);
  const temp = Number(stats.cpuTemp);
  const gpu = gpuOf(stats.gpu);
  const ram = clampPct(stats.memory?.percent);

  let cpuAlerting = false;
  let tempAlerting = false;
  let gpuAlerting = false;
  let ramAlerting = false;

  if (cpu > THRESHOLDS.cpu) {
    guardian.cpuStreak += 1;
    cpuAlerting = guardian.cpuStreak >= ALERT_STREAK_FOR_NOTIFICATION;
  } else {
    guardian.cpuStreak = 0;
    guardian.cpuFired = false;
  }

  // TEMP_INVALID (-1): sensor ausente → nunca alerta térmica.
  if (Number.isFinite(temp) && temp > TEMP_INVALID && temp >= THRESHOLDS.temp) {
    guardian.tempStreak += 1;
    tempAlerting = guardian.tempStreak >= ALERT_STREAK_FOR_NOTIFICATION;
  } else {
    guardian.tempStreak = 0;
    guardian.tempFired = false;
  }

  // GPU_INVALID (-1): contadores ausentes (typeperf sin 1ª muestra) → nunca alerta.
  if (gpu !== null && gpu >= THRESHOLDS.gpu) {
    guardian.gpuStreak += 1;
    gpuAlerting = guardian.gpuStreak >= ALERT_STREAK_FOR_NOTIFICATION;
  } else {
    guardian.gpuStreak = 0;
    guardian.gpuFired = false;
  }

  if (ram >= THRESHOLDS.ram) {
    guardian.ramStreak += 1;
    ramAlerting = guardian.ramStreak >= ALERT_STREAK_FOR_NOTIFICATION;
  } else {
    guardian.ramStreak = 0;
    guardian.ramFired = false;
  }

  // Notificación nativa: SOLO la 1ª vez por episodio (latch anti-flood),
  // sujeta al cooldown global de 60 s por tipo.
  if (cpuAlerting && !guardian.cpuFired) {
    guardian.cpuFired = true;
    notify('cpu', '⚠️ CPU Alert', `CPU load ${fmt1(cpu)}% sustained above ${THRESHOLDS.cpu}%`);
  }
  if (tempAlerting && !guardian.tempFired) {
    guardian.tempFired = true;
    notify('temp', '🌡️ Thermal Alert', `CPU temperature ${temp.toFixed(0)}°C sustained above ${THRESHOLDS.temp}°C`);
  }
  if (gpuAlerting && !guardian.gpuFired) {
    guardian.gpuFired = true;
    notify('gpu', '🎮 GPU Alert', `GPU usage ${fmt1(gpu)}% sustained above ${THRESHOLDS.gpu}%`);
  }
  if (ramAlerting && !guardian.ramFired) {
    const { usedGb, totalGb } = stats.memory ?? {};
    const detail =
      Number.isFinite(usedGb) && Number.isFinite(totalGb)
        ? ` (${usedGb.toFixed(1)}/${totalGb.toFixed(1)} GB)`
        : '';
    notify('ram', '🧠 RAM Alert', `Memory ${fmt1(ram)}% sustained above ${THRESHOLDS.ram}%${detail}`);
  }
}

// ---------------------------------------------------------------------------
// Renderizado del panel Dev (solo con modo 'dev' activo)
// ---------------------------------------------------------------------------
function renderDevPanel(stats) {
  if (!stats?.ok) return;

  const cpu = clampPct(stats.cpu);
  const ram = clampPct(stats.memory?.percent);
  const alert = cpu >= THRESHOLDS.cpu;

  el.cpuValue.textContent = fmt1(cpu);
  el.cpuBar.style.width = `${cpu}%`;
  el.cpuCard.classList.toggle('card--alert', alert);

  // Temperatura: 'n/a' si el sensor no existe; alerta visual ≥80 °C.
  const temp = Number(stats.cpuTemp);
  const tempValid = Number.isFinite(temp) && temp > TEMP_INVALID;
  el.tempValue.textContent = formatTemp(temp);
  if (tempValid) {
    el.tempBar.style.width = `${clampPct(temp)}%`;
    el.tempCard.classList.toggle('card--temp-alert', temp >= THRESHOLDS.temp);
  } else {
    el.tempBar.style.width = '0%';
    el.tempCard.classList.remove('card--temp-alert');
  }

  // GPU: barra/valor; 'n/a' hasta que typeperf produzca su 1ª muestra.
  const gpu = Number(stats.gpu);
  const gpuValid = Number.isFinite(gpu) && gpu > GPU_INVALID;
  el.gpuValue.textContent = formatGpu(gpu);
  if (gpuValid) {
    el.gpuBar.style.width = `${clampPct(gpu)}%`;
  } else {
    el.gpuBar.style.width = '0%';
  }

  el.ramValue.textContent = fmt1(ram);
  el.ramBar.style.width = `${ram}%`;
  el.ramCard?.classList.toggle('card--alert', ram >= THRESHOLDS.ram);
  const { usedGb, totalGb } = stats.memory ?? {};
  if (Number.isFinite(usedGb) && Number.isFinite(totalGb)) {
    el.ramDetail.textContent = `${usedGb.toFixed(2)} / ${totalGb.toFixed(2)} GB`;
  }

  el.netIface.textContent = stats.network?.iface ?? '';
  el.netDown.textContent = formatSpeed(stats.network?.rxBytesSec);
  el.netUp.textContent = formatSpeed(stats.network?.txBytesSec);
}

/** Tiras del mini overlay (solo con modo 'mini' activo). */
function renderMini(stats) {
  const cpu = clampPct(stats.cpu);
  const ram = clampPct(stats.memory?.percent);
  const temp = Number(stats.cpuTemp);
  const tempValid = Number.isFinite(temp) && temp > TEMP_INVALID;
  const gpu = Number(stats.gpu);
  const gpuValid = Number.isFinite(gpu) && gpu > GPU_INVALID;

  el.miniCpuBar.style.width = `${cpu}%`;
  el.miniCpuValue.textContent = fmt1(cpu);
  el.miniCpu.classList.toggle('strip--alert', cpu >= THRESHOLDS.cpu);

  el.miniRamBar.style.width = `${ram}%`;
  el.miniRamValue.textContent = fmt1(ram);
  el.miniRam.classList.toggle('strip--alert', ram >= THRESHOLDS.ram);

  el.miniTempBar.style.width = tempValid ? `${clampPct(temp)}%` : '0%';
  el.miniTempValue.textContent = formatTemp(temp);
  el.miniTempBar.closest('.strip')?.classList.toggle(
    'strip--alert',
    tempValid && temp >= THRESHOLDS.temp
  );

  el.miniGpuBar.style.width = gpuValid ? `${clampPct(gpu)}%` : '0%';
  el.miniGpuValue.textContent = formatGpu(gpu);
  el.miniGpuBar.closest('.strip')?.classList.toggle(
    'strip--alert',
    gpuValid && gpu >= THRESHOLDS.gpu
  );

  el.miniNetIface.textContent = stats.network?.iface ?? '';
  el.miniNetDown.textContent = formatSpeed(stats.network?.rxBytesSec);
  el.miniNetUp.textContent = formatSpeed(stats.network?.txBytesSec);
}

// ---------------------------------------------------------------------------
// Listas de procesos: filas persistentes reutilizadas (sin innerHTML nunca).
// Dos vistas de la MISMA data top-5: compacta (modo Dev) y completa (modo
// Procesos, con más aire por fila). Cada vista tiene su propio pool de filas.
// ---------------------------------------------------------------------------
function buildProcessRow(container) {
  const li = document.createElement('li');
  li.className = 'process-item';
  li.hidden = true;

  const name = document.createElement('span');
  name.className = 'process-item__name';
  const pid = document.createElement('span');
  pid.className = 'process-item__pid';
  const cpu = document.createElement('span');
  cpu.className = 'process-item__cpu';
  const kill = document.createElement('button');
  kill.className = 'kill-btn';
  kill.textContent = 'KILL';

  // El closure lee el PID vigente de la fila en el momento del clic.
  let currentPid = 0;
  kill.addEventListener('click', () => {
    if (currentPid > 0) handleKill(currentPid, kill);
  });

  li.append(name, pid, cpu, kill);
  container.appendChild(li);
  return { li, name, pid, cpu, kill, setPid: (v) => { currentPid = v; } };
}

function updateProcessRow(row, p) {
  row.setPid(p.pid);
  row.name.textContent = p.name || `PID ${p.pid}`;
  row.name.title = `${p.name} (PID ${p.pid})`;
  row.pid.textContent = `#${p.pid}`;
  row.cpu.textContent = `${fmt1(p.cpu)}%`;
  row.li.hidden = false;
}

const procRows = [];

function renderProcesses(list) {
  const hasData = Array.isArray(list) && list.length > 0;
  const listEl = el.processListFull;
  const emptyEl = el.listEmptyFull;
  const footEl = el.listFootTextFull;
  if (!listEl || !emptyEl || !footEl) return;

  emptyEl.hidden = hasData;
  footEl.textContent = hasData
    ? `muestreo cada ${(METRICS_INTERVAL_MS / 1000).toFixed(1)} s · top 5 por CPU`
    : '';

  if (!hasData) {
    listEl.replaceChildren();
    procRows.length = 0;
  } else {
    // Reutilización: las filas se crean una sola vez y se reciclan para siempre.
    while (procRows.length < list.length) procRows.push(buildProcessRow(listEl));
    for (let i = 0; i < procRows.length; i++) {
      if (i < list.length) updateProcessRow(procRows[i], list[i]);
      else procRows[i].li.hidden = true; // sobrantes ocultos, jamás destruidos
    }
  }
}

// ---------------------------------------------------------------------------
// Consulta de procesos: SOLO con modo Dev activo (y visible)
// ---------------------------------------------------------------------------
async function fetchProcesses(force = false) {
  if (!force && ((activeMode !== 'dev' && activeMode !== 'procs') || document.hidden)) return;
  try {
    const res = await window.api.getTopProcesses();
    renderProcesses(res?.ok ? res.processes : []);
  } catch {
    renderProcesses([]);
  }
}

// ---------------------------------------------------------------------------
// Acción Kill (confirmación + refresco inmediato)
// ---------------------------------------------------------------------------
async function handleKill(pid, button) {
  const proc = Number(pid);
  if (!Number.isInteger(proc) || proc <= 0) return;

  const confirmed = window.confirm(`Kill process with PID ${proc}?`);
  if (!confirmed) return;

  if (button) button.disabled = true;
  try {
    const res = await window.api.killProcess(proc);
    if (!res?.ok) {
      window.alert(`Could not kill PID ${proc}: ${res?.error ?? 'unknown error'}`);
    }
  } catch (error) {
    window.alert(`Could not kill PID ${proc}: ${error?.message ?? error}`);
  } finally {
    if (button) button.disabled = false;
    await fetchProcesses(true); // refresco inmediato tras confirmar
  }
}

// ---------------------------------------------------------------------------
// Info de GPU (modelo/driver/VRAM): una sola consulta al arrancar en modo Dev.
// ---------------------------------------------------------------------------
let gpuInfoFetched = false;
async function fetchGpuInfo() {
  if (gpuInfoFetched || !el.gpuDetail) return;
  gpuInfoFetched = true;
  try {
    const res = await window.api.getGpuInfo();
    if (res?.ok) {
      const bits = [res.model];
      if (res.vramMb) bits.push(`${res.vramMb} MB`);
      el.gpuDetail.textContent = bits.filter(Boolean).join(' · ');
    }
  } catch { /* la tarjeta muestra '--' */ }
}

// ---------------------------------------------------------------------------
// Loop de muestreo: UN solo intervalo de 2500 ms, trabajo filtrado por modo
// ---------------------------------------------------------------------------
let metricsTimer = null;

async function pollTick() {
  if (backendHidden || document.hidden) return; // Widget oculto en bandeja: cero trabajo.
  try {
    const stats = await window.api.getSystemStats();
    if (!stats?.ok) return;
    lastStats = stats;

    // El historial SIEMPRE avanza (coste trivial) para que las gráficas no
    // pierdan puntos al volver al modo Gráficas. TEMP/GPU sin dato → null:
    // la gráfica muestra un hueco real (spanGaps:false), no un cero falso.
    const netKib = (Number(stats.network?.rxBytesSec) + Number(stats.network?.txBytesSec)) / 1024;
    pushCapped(history.cpu, clampPct(stats.cpu));
    pushCapped(history.temp, tempOf(stats.cpuTemp));
    pushCapped(history.ram, clampPct(stats.memory?.percent));
    pushCapped(history.gpu, gpuOf(stats.gpu));
    pushCapped(history.net, netKib);

    // El guardián siempre vigila, esté el modo que esté.
    runGuardian(stats);

    // Hints honestos según el estado real del sensor térmico:
    //   'ok'    → ninguno.
    //   'admin' → sin datos + sin elevación (ambiguo: permisos o sin sensor).
    //   'none'  → sin datos incluso elevado: el hardware no publica sensor.
    if (el.tempAdminHint) el.tempAdminHint.hidden = stats.tempStatus !== 'admin';
    if (el.tempNoneHint) el.tempNoneHint.hidden = stats.tempStatus !== 'none';

    // Renderizado condicional estricto por modo.
    if (activeMode === 'dev') renderDevPanel(stats);
    else if (activeMode === 'mini') renderMini(stats);
    else if (activeMode === 'charts') updateCharts(stats);
    // En modo 'procs' las tarjetas no existen: solo la lista de abajo.

    // La lista de procesos SOLO se consulta en modo Dev o Procesos.
    if (activeMode === 'dev' || activeMode === 'procs') {
      await fetchProcesses();
      fetchGpuInfo(); // Cacheada tras la 1ª llamada; no repite IPC.
    }
  } catch (error) {
    console.error('[renderer] polling error:', error);
  }
}

/** Mapea cpuTemp → número gráfico o null (sin sensor). */
function tempOf(v) {
  const t = Number(v);
  return Number.isFinite(t) && t > TEMP_INVALID ? t : null;
}

/** Mapea gpu → número gráfico o null (sin contadores todavía). */
function gpuOf(v) {
  const g = Number(v);
  return Number.isFinite(g) && g > GPU_INVALID ? g : null;
}

function startPolling() {
  pollTick();
  metricsTimer = setInterval(pollTick, METRICS_INTERVAL_MS);
}

function stopPolling() {
  if (metricsTimer) clearInterval(metricsTimer);
  metricsTimer = null;
}

// ---------------------------------------------------------------------------
// Arranque y eventos
// ---------------------------------------------------------------------------
el.btnMini.addEventListener('click', () => setMode('mini'));
el.btnDev.addEventListener('click', () => setMode('dev'));
el.btnCharts.addEventListener('click', () => setMode('charts'));
el.btnProcs.addEventListener('click', () => setMode('procs'));
el.btnRefreshProcs?.addEventListener('click', () => fetchProcesses(true));

// Expandir desde el mini overlay (clic en CPU/RAM).
el.miniCpu.addEventListener('click', () => setMode('dev'));
el.miniRam.addEventListener('click', () => setMode('dev'));

el.btnPin.addEventListener('click', async () => {
  try {
    const res = await window.api.toggleAlwaysOnTop();
    if (res?.ok) el.btnPin.classList.toggle('icon-btn--active', Boolean(res.pinned));
  } catch { /* sin cambios si falla el IPC */ }
});

el.btnClose.addEventListener('click', () => {
  // Ocultar vía comando de Rust: window.close() está PROHIBIDO aquí. En
  // WebView2 wry lo responde destruyendo el HWND del webview sin pasar por el
  // CloseRequested de Tauri → ventana negra pegada que solo se arregla
  // reiniciando. hide_widget solo oculta: la app sigue viva en la bandeja.
  // El intervalo NO se detiene: pollTick ya no hace trabajo con el documento
  // oculto, y detenerlo aquí congelaba el widget al volver a mostrarlo.
  window.api?.hideWidget?.();
});

document.addEventListener('visibilitychange', () => {
  if (!document.hidden && !backendHidden) pollTick(); // refresco inmediato al salir de la bandeja
});

// WebView2 NO propaga document.hidden cuando la ventana anfitriona se oculta,
// así que el renderer sigue el evento del backend (fuente de verdad): oculto
// en bandeja, pollTick sale inmediatamente = cero IPC/DOM/Chart. Al volver,
// un refresco inmediato restaura los datos sin esperar el próximo tick.
let backendHidden = false;
window.api.onVisibilityChanged?.((visible) => {
  backendHidden = !visible;
  if (visible && !document.hidden) pollTick();
});

window.addEventListener('beforeunload', () => {
  stopPolling();
  for (const key of Object.keys(charts)) charts[key]?.destroy();
});

// Estado inicial del botón de fijado.
window.api.getAlwaysOnTop().then((res) => {
  if (res?.ok) el.btnPin.classList.toggle('icon-btn--active', Boolean(res.pinned));
}).catch(() => {});

// El backend es la fuente de verdad del modo: si el modo cambia sin pasar por
// la UI (llamada IPC directa de tests, atajos futuros), el renderer sigue.
// setMode ya aplicó el cambio cuando el clic viene de aquí, así que el guard
// evita trabajo duplicado.
window.api.onModeChanged?.((mode) => {
  if (typeof mode === 'string' && mode !== activeMode) setMode(mode, true);
});

// Arranque.
ensureModeBootstrap();
startPolling();
fetchProcesses(true);

/** Pinta el estado inicial coherente con el modo por defecto ('dev'). */
function ensureModeBootstrap() {
  applyModeUI(activeMode);
}
