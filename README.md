# System Monitor Widget

Floating minimalist desktop widget for real-time system monitoring in the corner of your screen: **CPU, RAM, Network, CPU temperature and GPU usage**. Built with **Tauri v2** (Rust backend + WebView2).

![License](https://img.shields.io/npm/l/system-monitor-widget)

## Features

- **4 display modes**: Mini-Overlay (compact), Dev & Diagnostics, real-time charts, Top 5 processes.
- **Accurate metrics**: CPU via kernel tick deltas, real RAM usage (`total − available`), per-interface virtual-adapter filtering, ACPI CPU temperature and GPU usage from Windows perf counters (PDH/`typeperf`) — same source as Task Manager.
- **Proactive guardian**: native Windows notifications when CPU ≥ 85%, RAM ≥ 90%, GPU ≥ 90% or CPU temp ≥ 80 °C sustained across reads (anti-flood, per-type cooldown).
- **Kill processes** from the Top 5 list with validation in Rust (never the widget itself, never system-critical PIDs).
- **Tray + global shortcut** `Ctrl+Shift+M` to show/hide, always-on-top toggle, single-instance lock.
- **Low footprint**: 2.5 s sampling, TTL + single-flight caching for expensive WMI queries, persistent GPU sampler (one `typeperf` child, no per-cycle process spawns). While hidden, samplers gate by visibility and `typeperf` is suspended via `NtSuspendProcess` — and a Job Object guarantees no orphaned `typeperf.exe` ever outlives the app.

## Requirements

- Windows 10/11 (primary target). The app also runs on Linux/macOS with reduced telemetry (no admin CPU temp).
- [Rust](https://rustup.rs) and the Tauri v2 prerequisites (WebView2 on Windows).

## Run from source

```bash
npm install
npm run tauri:dev
```

## Build & distribution

```bash
npm run tauri:build       # release binary (configure bundle targets in src-tauri/tauri.conf.json)
```

The binary is compiled natively in Rust: no Node runtime embedded, small footprint, and the web layer is sandboxed by WebView2 with a strict Content-Security-Policy.

## Testing

The suite has two layers:

| Command | What it runs | Needs the app live? |
|---|---|---|
| `npm test` | **23 unit tests** (`test/*.test.js`): cache TTL + single-flight, metrics formatting, PID validation, event listeners | No |
| `npm run test:e2e` | **39-check E2E suite** (`tauri-full-test.js`) over CDP: real UI rendering of the 4 modes, charts with data, process list, kill-process contract, hide-to-tray (OS-level visibility via `IsWindowVisible`), single instance | **Yes** |
| `cargo test` (in `src-tauri/`) | **15 Rust tests**: `mode-changed` event emission, hide/show state transitions and kill guards with `MockRuntime`, GPU CSV parsing, virtual-adapter filter | No |
| `node e2e-mode-event.js` | Focused E2E for live `onModeChanged` delivery: IPC path, UI path without event echo, `unlisten` | **Yes** |

### Running the E2E tests

The E2E harness talks to the app through the Chrome DevTools Protocol (CDP), so the app must be running with the remote-debugging port enabled:

```bash
# 1. Build once (or reuse dist/ if it is up to date)
npm run tauri:build

# 2. Launch the app with CDP enabled
cd dist
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9223" "./System Monitor Widget-Tauri-Portable-1.0.0.exe" &

# 3. Run the suites
cd ..
npm run test:e2e          # full suite (39 checks)
node e2e-mode-event.js    # onModeChanged delivery (8 checks)
```

Notes:
- WebView2 receives the debug port through the `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` env var — no code changes needed.
- The E2E suite probes real OS window visibility with `IsWindowVisible` (`win-visibility.ps1`) because WebView2 does not propagate `document.hidden` when the host window hides.
- Close the test instance afterwards (`Get-Process 'System Monitor Widget-Tauri-Portable-1.0.0' | Stop-Process`) so it doesn't fight the single-instance lock with your daily session.

### PowerShell utilities

| Script | Purpose |
|---|---|
| `hide.ps1` | Sends `WM_SYSCOMMAND` to the running widget to exercise the hide-to-tray path from scripts. |
| `measure.ps1` | Samples the widget's CPU usage over 60 s (plus `typeperf` presence) — used to verify the low-footprint claim. |
| `win-visibility.ps1` | Enumerates the process's top-level windows and reports `IsWindowVisible` per HWND — used by the E2E hide checks. |

## Controls

| Key / control | Action |
|---|---|
| `Ctrl+Shift+M` | Toggle widget visibility (global). |
| Tray icon (click) | Toggle visibility. |
| Pin button | Always-on-top on/off. |
| KILL button | Kill a process (confirm required). |

## License

[MIT](LICENSE)
