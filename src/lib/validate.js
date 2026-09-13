'use strict';

/**
 * validate.js — Validación de PIDs para kill-process.
 * Extraído de main.js para poder testearlo en Node sin arrancar Electron.
 */

/**
 * Valida un PID antes de matar un proceso.
 * @param {*} pid PID crudo desde el renderer (número o string).
 * @param {number} selfPid PID del propio widget (para impedir el suicidio).
 * @returns {{ok: true, pid: number} | {ok: false, error: string}}
 */
function validatePid(pid, selfPid) {
  const numericPid = Number(pid);
  if (!Number.isInteger(numericPid) || numericPid <= 0) {
    return { ok: false, error: 'Invalid PID' };
  }
  // Nunca dejar que el widget se suicide.
  if (numericPid === selfPid) {
    return { ok: false, error: 'Refusing to kill the widget itself' };
  }
  // PIDs críticos del SO: PID 4 = System (Windows), PID 1 = init/systemd (POSIX).
  // No debe matarse por la IU aunque el `taskkill` local falle por permisos.
  if ((process.platform === 'win32' && numericPid === 4) || (process.platform !== 'win32' && numericPid === 1)) {
    return { ok: false, error: 'Refusing to kill a system-critical process' };
  }
  return { ok: true, pid: numericPid };
}

module.exports = { validatePid };
