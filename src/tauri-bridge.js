'use strict';

/**
 * tauri-bridge.js — Puente Tauri (WebView2) con la MISMA superficie que el
 * preload de Electron (window.api), para que src/renderer.js funcione sin
 * cambios en ambos runtimes.
 *
 * - Bajo Electron este archivo es inerte: el preload ya expuso window.api y
 *   window.__TAURI_INTERNALS__ no existe.
 * - Bajo Tauri define window.api sobre invoke() y sustituye window.Notification
 *   (WebView2 no implementa la API HTML5 de notificaciones) por el plugin
 *   nativo tauri-plugin-notification.
 */
(function () {
  const internals = window.__TAURI_INTERNALS__;
  if (!internals) return; // Electron: nada que hacer aquí.

  const invoke = internals.invoke;

  /** Valida argumentos igual que el preload de Electron (defensa en profundidad). */
  const toPid = (value) => {
    const n = Number(value);
    return Number.isInteger(n) && n > 0 ? n : null;
  };

  const toMode = (value) =>
    typeof value === 'string' && ['dev', 'mini', 'charts', 'procs'].includes(value) ? value : null;

  window.api = {
    /** CPU %, RAM (% y GB), red (B/s) y temperatura °C. */
    getSystemStats: () => invoke('get_system_stats'),
    /** Top 5 procesos por CPU → [{ pid, name, cpu, mem }] */
    getTopProcesses: () => invoke('get_top_processes'),
    /** Termina un proceso por PID. */
    killProcess: (pid) => invoke('kill_process', { pid: toPid(pid) }),
    /** Fija / desfija el widget sobre las demás ventanas. → { ok, pinned } */
    toggleAlwaysOnTop: () => invoke('toggle_always_on_top'),
    /** Consulta el estado de fijado actual. → { ok, pinned } */
    getAlwaysOnTop: () => invoke('get_always_on_top'),
    /** Cambia el modo de visualización: 'mini' | 'dev' | 'charts' | 'procs'. */
    setWidgetMode: (mode) => invoke('set_widget_mode', { mode: toMode(mode) }),
    /** Metadatos de GPU: modelo, driver, VRAM (cacheado en Rust). */
    getGpuInfo: () => invoke('get_gpu_info'),
  };

  /**
   * WebView2 no implementa `new Notification()` (API HTML5): el renderer la
   * usa en el guardián de alertas. Se sustituye por un shim que emite la
   * notificación nativa de Windows vía tauri-plugin-notification.
   */
  window.Notification = class TauriNotification {
    static get permission() {
      return 'granted';
    }
    static requestPermission() {
      return Promise.resolve('granted');
    }
    constructor(title, options = {}) {
      this.title = String(title ?? '');
      this.body = String(options.body ?? '');
      this.onclick = null;
      invoke('plugin:notification|notify', {
        options: { title: this.title, body: this.body, silent: false },
      }).catch(() => {
        /* Entornos sin notificaciones: la alerta simplemente se omite. */
      });
    }
    close() {}
  };
})();