'use strict';

/**
 * cache.js — Caché TTL con single-flight para consultas caras (WMI, etc.).
 * Extraído de main.js para poder testearlo en Node sin arrancar Electron.
 *
 * - Dentro de la ventana TTL, las llamadas al MISMO key devuelven la MISMA
 *   promesa (single-flight): solo un query real está en vuelo a la vez.
 * - La limpieza es diferida: la entrada queda servible hasta la próxima
 *   ventana TTL y luego se elimina si sigue siendo la misma promesa.
 */

/** @type {Map<string, { at: number, promise: Promise<any> }>} */
const inflight = new Map();

async function cached(key, ttlMs, producer) {
  const now = Date.now();
  const entry = inflight.get(key);
  if (entry && now - entry.at < ttlMs) return entry.promise;
  const promise = producer().finally(() => {
    // Limpieza diferida: la promesa queda servible hasta la próxima ventana TTL.
    setTimeout(() => {
      const e = inflight.get(key);
      if (e && e.promise === promise) inflight.delete(key);
    }, ttlMs);
  });
  inflight.set(key, { at: now, promise });
  return promise;
}

/** Invalida una entrada (p. ej. tras matar un proceso, el top-5 debe refrescar). */
function invalidate(key) {
  inflight.delete(key);
}

module.exports = { cached, invalidate };
