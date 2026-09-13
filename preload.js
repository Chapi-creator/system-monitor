'use strict';

/**
 * preload.js — Puente seguro IPC via contextBridge.
 * contextIsolation: true + nodeIntegration: false ⇒ el renderer no ve Node.
 * No se expone ningún objeto de Node: solo funciones que devuelven promesas
 * hacia los manejadores de ipcMain.handle.
 */

const { contextBridge, ipcRenderer } = require('electron');

/**
 * Valida los argumentos antes de salir del puente: defensa en profundidad
 * contra payloads no numéricos que atraviesen el contexto aislado.
 */
const toPid = (value) => {
  const n = Number(value);
  return Number.isInteger(n) && n > 0 ? n : null;
};

const toMode = (value) =>
  typeof value === 'string' && ['dev', 'mini', 'charts', 'procs'].includes(value) ? value : null;

contextBridge.exposeInMainWorld('api', {
  /** CPU %, RAM (% y GB), red (B/s) y temperatura °C. */
  getSystemStats: () => ipcRenderer.invoke('get-system-stats'),
  /** Top 5 procesos por CPU → [{ pid, name, cpu, mem }] */
  getTopProcesses: () => ipcRenderer.invoke('get-top-processes'),
  /** Termina un proceso por PID. */
  killProcess: (pid) => ipcRenderer.invoke('kill-process', toPid(pid)),
  /** Fija / desfija el widget sobre las demás ventanas. → { ok, pinned } */
  toggleAlwaysOnTop: () => ipcRenderer.invoke('toggle-always-on-top'),
  /** Consulta el estado de fijado actual. → { ok, pinned } */
  getAlwaysOnTop: () => ipcRenderer.invoke('get-always-on-top'),
  /** Cambia el modo de visualización: 'mini' | 'dev' | 'charts'. */
  setWidgetMode: (mode) => ipcRenderer.invoke('set-widget-mode', toMode(mode)),
  /** Metadatos de GPU: modelo, driver, VRAM (cacheado en el proceso principal). */
  getGpuInfo: () => ipcRenderer.invoke('get-gpu-info'),
});
