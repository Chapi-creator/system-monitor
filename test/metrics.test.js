'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');

const { formatSpeed, pickActiveIface, parseGpuCsvLine } = require('../src/lib/metrics');

// ---------------------------------------------------------------------------
// formatSpeed
// ---------------------------------------------------------------------------
test('formatSpeed: convierte a KB/s por debajo de 1 MB/s', () => {
  assert.equal(formatSpeed(512 * 1024), '512.0 KB/s');
  assert.equal(formatSpeed(1024), '1.0 KB/s');
  assert.equal(formatSpeed(0), '0.0 KB/s');
});

test('formatSpeed: convierte a MB/s desde 1 MB/s', () => {
  assert.equal(formatSpeed(1024 * 1024), '1.00 MB/s');
  assert.equal(formatSpeed(2.5 * 1024 * 1024), '2.50 MB/s');
});

test('formatSpeed: valores no numéricos → "--"', () => {
  assert.equal(formatSpeed(undefined), '--');
  assert.equal(formatSpeed('abc'), '--');
  assert.equal(formatSpeed(Number.NaN), '--');
});

// ---------------------------------------------------------------------------
// pickActiveIface
// ---------------------------------------------------------------------------
test('pickActiveIface: lista vacía → iface n/a', () => {
  const r = pickActiveIface([]);
  assert.equal(r.iface, 'n/a');
  assert.equal(r.rx_sec, 0);
  assert.equal(r.tx_sec, 0);
});

test('pickActiveIface: elige la interfaz real activa con más tráfico (ignora virtuales)', () => {
  const net = [
    { iface: 'Ethernet', operstate: 'up', rx_sec: 1000, tx_sec: 500, rx_bytes: 100, tx_bytes: 50 },
    { iface: 'Wi-Fi', operstate: 'up', rx_sec: 3000, tx_sec: 2000, rx_bytes: 300, tx_bytes: 200 },
    { iface: 'vEthernet (WSL)', operstate: 'up', rx_sec: 9000, tx_sec: 9000, rx_bytes: 900, tx_bytes: 900 },
  ];
  assert.equal(pickActiveIface(net).iface, 'Wi-Fi');
});

test('pickActiveIface: sin tráfico instantáneo usa la de mayor tráfico acumulado', () => {
  const net = [
    { iface: 'Ethernet', operstate: 'up', rx_sec: 0, tx_sec: 0, rx_bytes: 100, tx_bytes: 50 },
    { iface: 'Wi-Fi', operstate: 'up', rx_sec: 0, tx_sec: 0, rx_bytes: 300, tx_bytes: 200 },
  ];
  assert.equal(pickActiveIface(net).iface, 'Wi-Fi');
});

test('pickActiveIface: última instancia elige la de mayor tráfico del listado completo', () => {
  const net = [
    { iface: 'lo', operstate: 'down', rx_sec: 0, tx_sec: 0, rx_bytes: 1, tx_bytes: 1 },
    { iface: 'Ethernet', operstate: 'down', rx_sec: 0, tx_sec: 0, rx_bytes: 50, tx_bytes: 25 },
  ];
  assert.equal(pickActiveIface(net).iface, 'Ethernet');
});

// ---------------------------------------------------------------------------
// parseGpuCsvLine
// ---------------------------------------------------------------------------
test('parseGpuCsvLine: línea de cabecera PDH-CSV → null', () => {
  assert.equal(parseGpuCsvLine('"(PDH-CSV 4.0.0.0)..."', ['3d']), null);
});

test('parseGpuCsvLine: línea vacía o nula → null', () => {
  assert.equal(parseGpuCsvLine('', null), null);
  assert.equal(parseGpuCsvLine(null, null), null);
});

test('parseGpuCsvLine: sin columnTypes → null', () => {
  assert.equal(parseGpuCsvLine('"ts","5"', null), null);
});

test('parseGpuCsvLine: solo timestamp (menos de 2 columnas) → null', () => {
  assert.equal(parseGpuCsvLine('"ts"', null), null);
});

test('parseGpuCsvLine: toma el máximo entre tipos de motor', () => {
  const line = '"02/13/2026 10:00:00.000","5","8","3"';
  const types = ['3d', 'copy', 'video'];
  assert.equal(parseGpuCsvLine(line, types), 8);
});

test('parseGpuCsvLine: suma motores del mismo engtype (pipeline 3D paralelo)', () => {
  const line = '"ts","5","3","0","0"';
  const types = ['3d', '3d', 'copy', 'video'];
  assert.equal(parseGpuCsvLine(line, types), 8); // 5 + 3
});

test('parseGpuCsvLine: recorta a 100% el uso agregado', () => {
  const line = '"ts","60","60","0"';
  const types = ['3d', '3d', 'copy'];
  assert.equal(parseGpuCsvLine(line, types), 100); // 60 + 60 = 120 → 100
});

test('parseGpuCsvLine: ignora valores no positivos', () => {
  const line = '"ts","0","-5","9"';
  const types = ['3d', 'copy', 'video'];
  assert.equal(parseGpuCsvLine(line, types), 9);
});
