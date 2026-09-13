'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');

const { cached } = require('../src/lib/cache');
const { validatePid } = require('../src/lib/validate');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------------------
// cached — caché TTL con single-flight
// ---------------------------------------------------------------------------
test('cached: ejecuta el productor una sola vez dentro del TTL', async () => {
  let calls = 0;
  const producer = async () => { calls += 1; return calls; };
  const a = await cached('sf-1', 100, producer);
  const b = await cached('sf-1', 100, producer);
  assert.equal(a, 1);
  assert.equal(b, 1);
  assert.equal(calls, 1, 'el productor debe ejecutarse una sola vez');
  await sleep(120); // dejar que expire la limpieza diferida
});

test('cached: re-produce tras vencer el TTL', async () => {
  let calls = 0;
  const producer = async () => { calls += 1; return calls; };
  const a = await cached('ttl-1', 50, producer);
  assert.equal(a, 1);
  await sleep(80);
  const b = await cached('ttl-1', 50, producer);
  assert.equal(b, 2);
  assert.equal(calls, 2);
  await sleep(80);
});

test('cached: no duplica una query en vuelo (concurrente)', async () => {
  let calls = 0;
  const producer = async () => { calls += 1; await sleep(30); return 'x'; };
  const p1 = cached('inflight-1', 200, producer); // inicia la query, sin await
  await sleep(5);
  const p2 = cached('inflight-1', 200, producer); // dentro de la ventana en vuelo
  assert.equal(await p1, 'x');
  assert.equal(await p2, 'x');
  assert.equal(calls, 1, 'la query concurrente no debe duplicarse');
  await sleep(220);
});

test('cached: un productor que falla se puede reintentar en el siguiente ciclo', async () => {
  let calls = 0;
  const producer = async () => {
    calls += 1;
    if (calls === 1) throw new Error('boom');
    return 'ok';
  };
  await assert.rejects(() => cached('err-1', 50, producer), /boom/);
  await sleep(80);
  const val = await cached('err-1', 50, producer);
  assert.equal(val, 'ok');
  assert.equal(calls, 2);
  await sleep(80);
});

// ---------------------------------------------------------------------------
// validatePid — validación de kill-process
// ---------------------------------------------------------------------------
test('validatePid: rechaza PIDs no válidos', () => {
  assert.deepEqual(validatePid('abc', 100), { ok: false, error: 'Invalid PID' });
  assert.deepEqual(validatePid(-1, 100), { ok: false, error: 'Invalid PID' });
  assert.deepEqual(validatePid(0, 100), { ok: false, error: 'Invalid PID' });
  assert.deepEqual(validatePid(1.5, 100), { ok: false, error: 'Invalid PID' });
  assert.deepEqual(validatePid('5.5', 100), { ok: false, error: 'Invalid PID' });
});

test('validatePid: rechaza matar el propio widget', () => {
  assert.deepEqual(validatePid(100, 100), { ok: false, error: 'Refusing to kill the widget itself' });
  assert.deepEqual(validatePid('100', 100), { ok: false, error: 'Refusing to kill the widget itself' });
});

test('validatePid: acepta un PID válido distinto', () => {
  assert.deepEqual(validatePid(1234, 100), { ok: true, pid: 1234 });
  assert.deepEqual(validatePid('1234', 100), { ok: true, pid: 1234 });
});

test('validatePid: rechaza PIDs críticos del sistema', () => {
  // PID 4 = System (Windows), PID 1 = init/systemd (POSIX).
  assert.deepEqual(validatePid(4, 100), { ok: false, error: 'Refusing to kill a system-critical process' });
  assert.deepEqual(validatePid('4', 100), { ok: false, error: 'Refusing to kill a system-critical process' });
});
