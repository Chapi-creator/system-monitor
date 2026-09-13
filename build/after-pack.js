'use strict';

/**
 * after-pack.js — Electron Fuses hardening aplicado al binario final.
 * Se ejecuta automáticamente al empaquetar (electron-builder → build.afterPack):
 *  - runAsNode off: no se puede arrancar el binario como Node genérico.
 *  - Node CLI inspect off: bloquea inyección de código vía --inspect/--inspect-brk.
 *  - NODE_OPTIONS off: bloquea inyección vía variables de entorno.
 *  - OnlyLoadAppFromAsar: el binario solo ejecuta el app.asar firmado por el build
 *    (impide que un atacante ponga otro JS en el path del app).
 */

const path = require('node:path');
const { flipFuses, FuseVersion, FuseV1Options } = require('@electron/fuses');

exports.default = async function afterPack(context) {
  const exe = path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.exe`);
  await flipFuses(exe, {
    version: FuseVersion.V1,
    [FuseV1Options.RunAsNode]: false,
    [FuseV1Options.EnableNodeCliInspectArguments]: false,
    [FuseV1Options.EnableNodeOptionsEnvironmentVariable]: false,
    [FuseV1Options.OnlyLoadAppFromAsar]: true,
  });
};