'use strict';

/**
 * tauri-bridge.js — Puente Tauri (WebView2) que expone window.api al renderer,
 * con validación de argumentos en profundidad antes de llegar a Rust.
 *
 * - Define window.api sobre invoke().
 * - Sustituye window.Notification (WebView2 no implementa la API HTML5 de
 *   notificaciones) por el plugin nativo tauri-plugin-notification.
 */
(function () {
  const internals = window.__TAURI_INTERNALS__;
  if (!internals) return; // Fuera de Tauri: nada que hacer aquí.

  const invoke = internals.invoke;

  /** Valida argumentos antes de invocar a Rust (defensa en profundidad). */
  const toPid = (value) => {
    const n = Number(value);
    return Number.isInteger(n) && n > 0 ? n : null;
  };

  const toMode = (value) =>
    typeof value === 'string' && ['dev', 'mini', 'charts', 'procs'].includes(value) ? value : null;

  window.api = {
    /** CPU %, RAM (% y GB), red (B/s), disco (B/s) y temperatura °C. */
    getSystemStats: () => invoke('get_system_stats'),
    /** Top 5 procesos por CPU → [{ pid, name, cpu, mem, exe? }] */
    getTopProcesses: () => invoke('get_top_processes'),
    /** Máximos y promedios desde el arranque (calculados en Rust). */
    getSessionStats: () => invoke('get_session_stats'),
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
    /** Preferencias persistidas (modo, pin, umbrales, posición). */
    getSettings: () => invoke('get_settings'),
    /**
     * Ajusta un umbral del guardián: kind 'cpu'|'ram'|'gpu'|'temp', valor
     * absoluto finito (Rust lo clampa a su rango). → { ok, thresholds }
     */
    setThreshold: (kind, value) => {
      const k = typeof kind === 'string' && ['cpu', 'ram', 'gpu', 'temp'].includes(kind) ? kind : null;
      const v = Number(value);
      return invoke('set_threshold', { kind: k, value: Number.isFinite(v) ? v : null });
    },
    /**
     * Aplica un DESFASE (±delta) al umbral: el backend suma al valor vigente.
     * Es lo que usan los steppers del overlay de Ajustes. → { ok, thresholds }
     */
    setThresholdDelta: (kind, delta) => {
      const k = typeof kind === 'string' && ['cpu', 'ram', 'gpu', 'temp'].includes(kind) ? kind : null;
      const d = Number(delta);
      return invoke('set_threshold_delta', { kind: k, delta: Number.isFinite(d) ? d : null });
    },
    /**
     * Oculta la ventana a la bandeja SIN destruir el webview.
     * NUNCA usar window.close() aquí: wry (WebView2) responde destruyendo el
     * HWND del webview sin pasar por CloseRequested → ventana negra pegada.
     */
    hideWidget: () => invoke('hide_widget'),
    /** Vuelve a mostrar el widget desde la bandeja. */
    showWidget: () => invoke('show_widget'),
    /**
     * Suscripción a cambios de visibilidad reales (backend = fuente de verdad).
     * Devuelve una promesa que resuelve al eventId para poder hacer unlisten.
     */
    onVisibilityChanged: (cb) => {
      const handler = internals.transformCallback((event) => cb(event?.payload));
      return invoke('plugin:event|listen', {
        event: 'visibility-changed',
        target: { kind: 'Any' },
        handler,
      });
    },
    /**
     * Suscripción a cambios de modo originados en el backend (el backend es la
     * fuente de verdad): devuelve una función unlisten.
     */
    onModeChanged: (cb) => {
      const handler = internals.transformCallback((event) => cb(event?.payload));
      return invoke('plugin:event|listen', {
        event: 'mode-changed',
        target: { kind: 'Any' },
        handler,
      });
    },
    /**
     * Suscripción a cambios de umbrales (backend = fuente de verdad): cubre
     * ajustes por IPC directo o desde el binario nativo.
     */
    onThresholdsChanged: (cb) => {
      const handler = internals.transformCallback((event) => cb(event?.payload));
      return invoke('plugin:event|listen', {
        event: 'thresholds-changed',
        target: { kind: 'Any' },
        handler,
      });
    },
  };

  /**
   * DEFENSA EN PROFUNDIDAD: en WebView2 wry responde a window.close()
   * destruyendo el HWND del webview (add_WindowCloseRequested → DestroyWindow)
   * SIN pasar por el CloseRequested de Tauri, así que prevent_close() + hide()
   * nunca se ejecutan y la ventana queda NEGRA y pegada hasta reiniciar.
   * Se re-mapea a hide_widget: mismo efecto visible (va a bandeja) sin destruir
   * nada. Así ninguna ruta futura del renderer puede recaer en el bug.
   */
  window.close = () => {
    invoke('hide_widget').catch(() => { /* sin ventana: nada que hacer */ });
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