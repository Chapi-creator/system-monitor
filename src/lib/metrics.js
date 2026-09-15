'use strict';

/**
 * metrics.js — Utilidades PURAS de cálculo/formato compartidas entre el
 * backend (Rust) y el renderer (renderer.js).
 *
 * Formato UMD:
 *  - En Node se exporta vía module.exports → require(...).
 *  - En el navegador (WebView del widget) se cuelga en window.SysMonMetrics.
 * Así las mismas funciones se pueden testear con node:test sin arrancar la app
 * y se usan en vivo desde el renderer sin duplicar código.
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) {
    module.exports = factory();
  } else {
    root.SysMonMetrics = factory();
  }
})(typeof self !== 'undefined' ? self : this, function () {
  'use strict';

  /** Convierte a número finito; cualquier cosa no numérica → 0. */
  const toNumber = (value) => {
    const n = Number(value);
    return Number.isFinite(n) ? n : 0;
  };

  /** Redondeo a 1 decimal — el formato de precisión de todas las métricas. */
  const round1 = (n) => Math.round(toNumber(n) * 10) / 10;
  /** Redondeo a 2 decimales — reservado para magnitudes en GB. */
  const round2 = (n) => Math.round(toNumber(n) * 100) / 100;

  /**
   * Conversión dinámica exacta de velocidad de red:
   * <1 MB/s → KB/s (÷1024, 1 decimal); ≥1 MB/s → MB/s (2 decimales).
   * @param {number} bytesSec
   * @returns {string}
   */
  function formatSpeed(bytesSec) {
    const bytes = Number(bytesSec);
    if (!Number.isFinite(bytes)) return '--';
    if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(2)} MB/s`;
    return `${(bytes / 1024).toFixed(1)} KB/s`;
  }

  /**
   * Velocidad de disco legible: mismo formato dinámico que la red (KB/s →
   * MB/s). Sentinel -1 (contadores aún no disponibles) → 'n/a'.
   * @param {number} bytesSec
   * @returns {string}
   */
  function formatDiskSpeed(bytesSec) {
    const bytes = Number(bytesSec);
    if (!Number.isFinite(bytes) || bytes < 0) return 'n/a';
    return formatSpeed(bytes);
  }

  /**
   * Acorta la ruta de un ejecutable para el tooltip del Top-5: conserva los
   * 2 últimos segmentos con su separador original (…\Local\app.exe).
   * Rutas cortas (≤ 2 separadores: unidad + 1 carpeta + exe) y vacías/null
   * se devuelven tal cual o con el texto de reserva; el title nativo siempre
   * muestra la ruta completa.
   * @param {string|null|undefined} exe Ruta completa o null (SO la oculta).
   * @returns {string}
   */
  function formatExePath(exe) {
    const s = String(exe ?? '').trim();
    if (!s) return 'ruta no disponible';
    const norm = s.replace(/\\/g, '/');
    const seps = norm.split('/').length - 1;
    if (seps <= 2) return s; // Ruta corta: entra completa.
    const slash = norm.lastIndexOf('/');
    const prevSlash = norm.lastIndexOf('/', slash - 1);
    // El reemplazo de separadores es 1:1: prevSlash apunta al mismo carácter
    // en `s`, y desde ahí se conserva "\carpeta\exe" íntegro.
    return '…' + s.slice(prevSlash);
  }

  /** Adaptadores virtuales a omitir (Docker, Hyper-V/WSL, VMware, VPN-TAP, loopback...). */
  const VIRTUAL_IFACE_RE = /(docker|vethernet|vmware|virtualbox|loopback|wsl|hyper-v|tap|tun|bluetooth|hibernate|teredo)/i;

  /**
   * Selecciona la interfaz de red ACTIVA con precisión:
   * 1. operstate 'up' + tráfico instantáneo real (rx_sec/tx_sec > 0), excluyendo
   *    adaptadores virtuales → gana la de mayor ancho de banda instantáneo.
   * 2. Fallback: 'up' sin tráfico (sistema idle) → la de mayor tráfico acumulado.
   * 3. Último recurso: la de mayor tráfico acumulado del listado completo.
   */
  function pickActiveIface(net) {
    const REAL = (i) => i && i.iface && !VIRTUAL_IFACE_RE.test(String(i.iface));
    const total = (i, a, b) => toNumber(i[a]) + toNumber(i[b]);
    const busiest = (arr, a, b) => arr.reduce((best, i) => (total(i, a, b) > total(best, a, b) ? i : best));

    if (!Array.isArray(net) || net.length === 0) {
      return { iface: 'n/a', rx_sec: 0, tx_sec: 0, rx_bytes: 0, tx_bytes: 0 };
    }
    const active = net.filter((i) => REAL(i) && i.operstate === 'up' && (toNumber(i.rx_sec) > 0 || toNumber(i.tx_sec) > 0));
    if (active.length > 0) return busiest(active, 'rx_sec', 'tx_sec');
    const up = net.filter((i) => REAL(i) && i.operstate === 'up');
    if (up.length > 0) return busiest(up, 'rx_bytes', 'tx_bytes');
    return busiest(net, 'rx_bytes', 'tx_bytes');
  }

  /**
   * Parsea una línea CSV de typeperf → uso agregado de GPU en %.
   * AGREGACIÓN ESTILO ADMINISTRADOR DE TAREAS: los contadores son POR MOTOR
   * (3D, Copy, VideoDecode, GDI...) y cada motor reporta 0-100% de SÍ MISMO.
   * 1) Se suman los motores del mismo engtype (varios motores 3D en paralelo
   *    = uso real del pipeline 3D).
   * 2) Se toma el MÁXIMO entre tipos de motor: la GPU ejecuta colas distintas
   *    en depuraciones separadas, y el % del chip es el del tipo más ocupado.
   * (El promedio plano sobre las ~277 instancias diluye el uso real a ~0:
   *  8% real de 3D se mostraba como 0.01%.)
   * @param {string|null} line Línea CSV cruda de typeperf.
   * @param {string[]|null} columnTypes engtype por columna (sin el timestamp).
   * @returns {number|null}
   */
  function parseGpuCsvLine(line, columnTypes) {
    if (!line || line[0] !== '"') return null;
    const cols = [];
    let cur = '';
    let inQuotes = false;
    for (let i = 0; i < line.length; i++) {
      const ch = line[i];
      if (ch === '"') { inQuotes = !inQuotes; continue; }
      if (ch === ',' && !inQuotes) { cols.push(cur); cur = ''; continue; }
      cur += ch;
    }
    cols.push(cur);
    if (cols.length < 2) return null;
    if (!cols[0] || cols[0].startsWith('(PDH-CSV')) return null; // Cabecera.
    if (!columnTypes) return null; // Aún sin cabecera: no hay cómo clasificar.
    const byType = Object.create(null);
    for (let i = 1; i < cols.length; i++) {
      const v = Number(cols[i]);
      if (!Number.isFinite(v) || v <= 0) continue;
      const t = columnTypes[i - 1] ?? 'unknown';
      byType[t] = (byType[t] ?? 0) + v;
    }
    let max = 0;
    for (const k of Object.keys(byType)) {
      if (byType[k] > max) max = byType[k];
    }
    return Math.max(0, Math.min(100, max));
  }

  return { toNumber, round1, round2, formatSpeed, formatDiskSpeed, formatExePath, pickActiveIface, parseGpuCsvLine };
});
