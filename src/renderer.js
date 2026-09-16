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
const { formatSpeed, formatDiskSpeed, formatExePath } = window.SysMonMetrics;

const METRICS_INTERVAL_MS = 2500;  // Especificación: 2.5 s por ciclo.
const MAX_POINTS = 20;             // Límite estricto del historial de gráficas.

// Umbrales del guardián proactivo: TODAS las métricas cubiertas.
// Centralizados en un solo objeto para no repetir valores hardcodeados en el
// guardián, el renderizado Dev y el mini overlay.
// MUTABLE: los steppers del overlay Ajustes los actualizan vía
// set_threshold en Rust (que persiste y clampa); el guardián y el coloreado
// de alertas siempre leen los umbrales vigentes.
const THRESHOLDS = {
  cpu: 85,   // % de CPU sostenida.
  temp: 80,  // °C del paquete CPU.
  gpu: 90,   // % de GPU sostenida.
  ram: 90,   // % de RAM sostenida.
};
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
  btnSettings: document.getElementById('btn-settings'),
  btnClose: document.getElementById('btn-close'),
  // Overlay de Ajustes
  settingsOverlay: document.getElementById('settings-overlay'),
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
  // Disco (solo modo Dev)
  diskRead: document.getElementById('disk-read'),
  diskWrite: document.getElementById('disk-write'),
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
  cDisk: document.getElementById('c-disk'),
  chartCpu: document.getElementById('chart-cpu'),
  chartTemp: document.getElementById('chart-temp'),
  chartRam: document.getElementById('chart-ram'),
  chartGpu: document.getElementById('chart-gpu'),
  chartNet: document.getElementById('chart-net'),
  chartDisk: document.getElementById('chart-disk'),
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
  if (el.btnSettings) el.btnSettings.classList.toggle('icon-btn--active', el.settingsOverlay && !el.settingsOverlay.hidden);
}

/**
 * Ajusta un umbral vía IPC y repinta el overlay de Ajustes.
 * @param {string} kind 'cpu' | 'ram' | 'gpu' | 'temp'
 * @param {number} delta DESFASE a aplicar (±5), NO valor absoluto: el backend
 *   lo suma al valor vigente y clampa, así el stepper funciona igual aunque el
 *   usuario ya haya movido el umbral en otra sesión.
 */
async function adjustThreshold(kind, delta) {
  try {
    const res = await window.api.setThresholdDelta(kind, delta);
    if (res?.ok && res.thresholds) {
      Object.assign(THRESHOLDS, res.thresholds);
      renderSettingsValues();
    }
  } catch { /* sin backend (fuera de Tauri): ignorado */ }
}

/** Repinta los valores del overlay con los umbrales vigentes. */
function renderSettingsValues() {
  // Solo si existe Y está abierto (llamado también desde onThresholdsChanged,
  // que corre en cada cambio aunque el overlay esté cerrado u oculto).
  if (!el.settingsOverlay || el.settingsOverlay.hidden) return;
  const set = (id, v, suf) => {
    const node = document.getElementById(id);
    if (node) node.textContent = `${v}${suf}`;
  };
  set('th-cpu-val', Math.round(THRESHOLDS.cpu), '%');
  set('th-ram-val', Math.round(THRESHOLDS.ram), '%');
  set('th-gpu-val', Math.round(THRESHOLDS.gpu), '%');
  set('th-temp-val', Math.round(THRESHOLDS.temp), '°C');
}

function toggleSettings(show = el.settingsOverlay.hidden) {
  // Guard: sin overlay (markup no cargado o modo sin cuerpo) no hay nada que
  // hacer; sin este check un click del ⚙ en un estado raro lanzaba TypeError.
  if (!el.settingsOverlay || !el.btnSettings) return;
  el.settingsOverlay.hidden = !show;
  el.btnSettings.classList.toggle('icon-btn--active', show);
  if (show) renderSettingsValues();
}

// Clicks del overlay de Ajustes: cada botón ajusta ± y Rust persiste solo.
el.settingsOverlay?.addEventListener('click', (e) => {
  const btn = e.target.closest('button[data-th]');
  if (btn) {
    const { th, step } = btn.dataset;
    adjustThreshold(th, Number(step));
    return;
  }
  if (e.target.closest('#settings-close')) toggleSettings(false);
});

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
const history = { cpu: [], temp: [], ram: [], gpu: [], net: [], diskRead: [], diskWrite: [] };

/** Techo de la escala Y del chart GPU: nunca por debajo de 10%. */
const GPU_Y_FLOOR = 10;

function pushCapped(arr, value) {
  arr.push(value);
  if (arr.length > MAX_POINTS) arr.shift(); // shift() al superar los 20 puntos.
}

const charts = { cpu: null, temp: null, ram: null, gpu: null, net: null, disk: null };

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
  // Disco: dos series (lectura/escritura) en KiB/s, escala Y dinámica igual que NET.
  charts.disk = makeChart(el.chartDisk, 'DISCO', '#00ff88', 'rgba(0,255,136,0.12)', 64);
  charts.disk.data.datasets.push({
    ...baseDataset('ESCRITURA', '#00c8ff', 'rgba(0,200,255,0.10)'),
    data: history.diskWrite,
  });

  // Referencias vivas al historial (mapa explícito: el disco son DOS series y
  // un bucle por claves de `history` lanzaría TypeError en diskRead/diskWrite).
  charts.cpu.data.datasets[0].data = history.cpu;
  charts.temp.data.datasets[0].data = history.temp;
  charts.ram.data.datasets[0].data = history.ram;
  charts.gpu.data.datasets[0].data = history.gpu;
  charts.net.data.datasets[0].data = history.net;
  charts.disk.data.datasets[0].data = history.diskRead;
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
  // Disco: escala Y al pico reciente de lectura/escritura (mínimo 64 KiB/s).
  const diskChart = charts.disk;
  if (diskChart) {
    const peak = Math.max(
      ...history.diskRead.filter((v) => v !== null),
      ...history.diskWrite.filter((v) => v !== null),
      64
    );
    diskChart.options.scales.y.suggestedMax = peak;
  }

  // update('none'): sin animación, sin re-layout extra.
  for (const key of Object.keys(charts)) charts[key]?.update('none');

  el.cCpu.textContent = fmt1(clampPct(stats.cpu));
  el.cTemp.textContent = formatTemp(stats.cpuTemp);
  el.cRam.textContent = fmt1(clampPct(stats.memory?.percent));
  el.cGpu.textContent = formatGpu(stats.gpu);
  el.cNet.textContent = netKib.toFixed(1);
  const diskRead = Number(stats.disk?.readBytesSec);
  const diskWrite = Number(stats.disk?.writeBytesSec);
  el.cDisk.textContent =
    `${formatDiskSpeed(diskRead)} / ${formatDiskSpeed(diskWrite)}`;
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

  // Disco: 'n/a' mientras el 1er renglón PDH llega (~2 s tras el arranque).
  if (el.diskRead) el.diskRead.textContent = formatDiskSpeed(stats.disk?.readBytesSec);
  if (el.diskWrite) el.diskWrite.textContent = formatDiskSpeed(stats.disk?.writeBytesSec);
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
  row.pid.textContent = `#${p.pid}`;
  row.cpu.textContent = `${fmt1(p.cpu)}%`;
  row.li.hidden = false;
  // Tooltip nativo (DOM puro, sin innerHTML: la ruta/nunca puede inyectar
  // markup): nombre, PID, ruta del exe y consumo de la fila.
  row.li.title = buildProcessTooltip(p);
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
// Tooltip del Top-5: título con nombre/PID/ruta/CPU/RAM de cada proceso.
// ---------------------------------------------------------------------------
function buildProcessTooltip(p) {
  const exe = p.exe ? formatExePath(p.exe) : 'ruta no disponible';
  return [
    p.name || `PID ${p.pid}`,
    `PID: ${p.pid}`,
    exe,
    `CPU: ${fmt1(p.cpu)}%  ·  RAM: ${fmt1(p.mem)}%`,
  ].join('\n');
}

// ---------------------------------------------------------------------------
// Estadísticas de sesión (máx/prom desde el arranque): se consultan solo al
// abrir el modo Gráficas y cada tick siguiente; el cálculo vive en Rust.
// ---------------------------------------------------------------------------
let sessionStats = null;

// Nodos de session-stats cacheados: 11 querySelector por tick → 0.
// isConnected re-consulta si el DOM los reconstruyera (barato y a prueba
// de paneles re-renderizados).
const sessionNodeCache = {};
function sessionNode(sel) {
  let n = sessionNodeCache[sel];
  if (!n || !n.isConnected) {
    n = document.querySelector(sel);
    sessionNodeCache[sel] = n;
  }
  return n;
}

async function fetchSessionStats() {
  try {
    const res = await window.api.getSessionStats();
    if (!res?.ok) return;
    sessionStats = res;
    const q = (sel) => sessionNode(sel);
    const set = (sel, v, suffix) => {
      const node = q(sel);
      if (node) node.textContent = v == null ? '--' : `${v}${suffix ?? ''}`;
    };
    set('.session-cpu-max', res.cpu?.max, '%');
    set('.session-cpu-avg', res.cpu?.avg, '%');
    set('.session-ram-max', res.mem?.max, '%');
    set('.session-ram-avg', res.mem?.avg, '%');
    set('.session-gpu-max', res.gpu?.max, '%');
    set('.session-gpu-avg', res.gpu?.avg, '%');
    set('.session-net-max', res.net?.max != null ? formatSpeed(res.net.max) : null);
    set('.session-net-avg', res.net?.avg != null ? formatSpeed(res.net.avg) : null);
    const foot = q('.session-foot');
    if (foot) foot.textContent = `desde el arranque · ${res.samples} muestras`;
  } catch { /* la tarjeta muestra '--' */ }
}

// ---------------------------------------------------------------------------
// Loop de muestreo: UN solo intervalo de 2500 ms, trabajo filtrado por modo
// ---------------------------------------------------------------------------
let metricsTimer = null;
let ticking = false; // Anti-solape: un tick lento no encadena otro encima.

async function pollTick() {
  if (backendHidden || document.hidden) return; // Widget oculto en bandeja: cero trabajo.
  if (ticking) return; // Tick anterior aún en vuelo: se salta, no se encadena.
  ticking = true;
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
    // Disco en KiB/s (sentinel -1 → null: hueco honesto, no cero falso).
    const diskReadKib = Number(stats.disk?.readBytesSec);
    const diskWriteKib = Number(stats.disk?.writeBytesSec);
    pushCapped(history.diskRead, diskReadKib >= 0 ? diskReadKib / 1024 : null);
    pushCapped(history.diskWrite, diskWriteKib >= 0 ? diskWriteKib / 1024 : null);

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
    else if (activeMode === 'charts') {
      updateCharts(stats);
      fetchSessionStats(); // máx/prom desde el arranque (barato: ya calculado)
    }
    // En modo 'procs' las tarjetas no existen: solo la lista de abajo.

    // La lista de procesos SOLO se consulta en modo Dev o Procesos.
    if (activeMode === 'dev' || activeMode === 'procs') {
      await fetchProcesses();
      fetchGpuInfo(); // Cacheada tras la 1ª llamada; no repite IPC.
    }
  } catch (error) {
    console.error('[renderer] polling error:', error);
  } finally {
    ticking = false; // Siempre se libera: el próximo intervalo vuelve a correr.
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
el.btnSettings?.addEventListener('click', () => toggleSettings());

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

// Cambios directos por IPC (tests, atajos) o desde el binario nativo: el
// guardián del renderer sigue los umbrales vigentes sin recargar.
// Campo a campo (no Object.assign): el payload puede venir parcial y un
// `undefined` no debe pisar un umbral válido. Si el cambio lo originó este
// renderer (adjustThreshold), re-aplicar los mismos valores es inofensivo.
window.api.onThresholdsChanged?.((th) => {
  if (!th || typeof th !== 'object') return;
  if (Number.isFinite(Number(th.cpu))) THRESHOLDS.cpu = Number(th.cpu);
  if (Number.isFinite(Number(th.ram))) THRESHOLDS.ram = Number(th.ram);
  if (Number.isFinite(Number(th.gpu))) THRESHOLDS.gpu = Number(th.gpu);
  if (Number.isFinite(Number(th.temp))) THRESHOLDS.temp = Number(th.temp);
  renderSettingsValues();
});

// Arranque.
ensureModeBootstrap();

// Preferencias persistidas: modo con el que se cerró y umbrales del guardián.
// El backend ya restauró pin/posición; el modo lo aplica aquí el renderer.
window.api.getSettings?.()
  .then((res) => {
    const st = res?.settings;
    if (!st) return;
    if (typeof st.thresholds === 'object' && st.thresholds) Object.assign(THRESHOLDS, st.thresholds);
    if (typeof st.mode === 'string' && st.mode !== activeMode) setMode(st.mode);
  })
  .catch(() => {});

startPolling();
fetchProcesses(true);

/** Pinta el estado inicial coherente con el modo por defecto ('dev'). */
function ensureModeBootstrap() {
  applyModeUI(activeMode);
}
