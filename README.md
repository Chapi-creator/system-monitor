# System Monitor Widget

Floating minimalist desktop widget for real-time system monitoring in the corner of your screen: **CPU, RAM, Network, CPU temperature and GPU usage**. Built with Electron.

![License](https://img.shields.io/badge/license-MIT-blue.svg)

## Features

- **4 display modes**: Mini-Overlay (compact), Dev & Diagnostics, real-time charts, Top 5 processes.
- **Accurate metrics**: CPU via kernel tick deltas, real RAM usage (`total − available`), per-interface network deltas, ACPI CPU temperature and GPU usage from Windows perf counters (PDH/`typeperf`) — same source as Task Manager.
- **Proactive guardian**: native Windows notifications when CPU ≥ 85%, RAM ≥ 90%, GPU ≥ 90% or CPU temp ≥ 80 °C sustained across reads (anti-flood, per-type cooldown).
- **Kill processes** from the Top 5 list with shell-level validation (never the widget itself, never system-critical PIDs).
- **Tray + global shortcut** `Ctrl+Shift+M` to show/hide, always-on-top toggle, single-instance lock.
- **Low footprint**: 2.5 s sampling, TTL + single-flight caching for expensive WMI queries, persistent GPU sampler (one `typeperf` child, no per-cycle process spawns).

## Requirements

- Windows 10/11 (primary target). The app also runs on Linux/macOS with reduced telemetry (no admin CPU temp).

## Run from source

```bash
npm install
npm start
```

## Build & distribution

```bash
npm run dist              # NSIS installer + portable (x64)
npm run dist:portable     # portable only
```

The build applies **Electron Fuses hardening** to the produced binary (`build/after-pack.js`): `runAsNode` disabled, `--inspect` arguments and `NODE_OPTIONS` injection disabled, and the app only loads from its signed `app.asar`.

## Security

- Renderer fully isolated: `contextIsolation: true`, `nodeIntegration: false`, `sandbox: true`.
- Strict Content-Security-Policy, no remote content, all rendering via `textContent` (no templates/innerHTML).
- IPC sender validation on state-changing handlers, PID validation on `kill-process`, navigation and window-open blocked.
- Dependencies pinned and audited (`npm audit` clean).

## Controls

| Key / control | Action |
|---|---|
| `Ctrl+Shift+M` | Toggle widget visibility (global). |
| Tray icon (click) | Toggle visibility. |
| Pin button | Always-on-top on/off. |
| KILL button | Kill a process (confirm required). |

## License

[MIT](LICENSE)