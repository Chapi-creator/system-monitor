//! main.rs — Proceso principal del widget en Tauri (Rust).
//!
//! Port de la versión Electron (main.js) a Tauri v2 + WebView2:
//!  - Ventana frameless, always-on-top, 340×440 (mini 340×245).
//!  - Bandeja (Tray) + atajo global Ctrl+Shift+M para mostrar/ocultar.
//!  - Instancia única (segundo lanzamiento enfoca la ventana existente).
//!  - 7 comandos IPC idénticos a los de Electron para que el renderer
//!    (src/*) funcione sin cambios, vía src/tauri-bridge.js.
//!  - Muestreo en segundo plano: CPU/RAM/red (sysinfo), GPU (typeperf/PDH),
//!    temperatura CPU (WMI MSAcpi_ThermalZoneTemperature) con estado honesto
//!    de 3 niveles ('ok' | 'admin' | 'none') y metadatos de GPU (WMI).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod sampler;

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, LogicalSize, Manager, State, WindowEvent};
use tauri_plugin_global_shortcut::ShortcutState;

// ---------------------------------------------------------------------------
// Estado compartido entre los hilos de muestreo y los comandos IPC.
// ---------------------------------------------------------------------------
const WIDGET_W: f64 = 340.0;
const WIDGET_H: f64 = 440.0;
const MINI_H: f64 = 245.0;

/// Snapshot de métricas con los MISMOS nombres que devolvía Electron
/// (camelCase) para no tocar el renderer.
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryInfo {
    pub percent: f64,
    pub used_gb: f64,
    pub total_gb: f64,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInfo {
    pub iface: String,
    pub rx_bytes_sec: f64,
    pub tx_bytes_sec: f64,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub ok: bool,
    pub cpu: f64,
    pub memory: MemoryInfo,
    pub network: NetworkInfo,
    pub cpu_temp: f64,
    pub temp_status: String,
    pub gpu: f64,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cpu: f64,
    pub mem: f64,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    pub ok: bool,
    pub model: String,
    pub vendor: String,
    pub vram_mb: Option<u64>,
    pub driver: String,
}

pub struct TempState {
    pub celsius: f64,
    pub status: String,
}

pub struct GpuInfoCache {
    pub at: Instant,
    pub info: GpuInfo,
}

/// Estado global accesible por comandos (tauri State) y por los hilos.
pub struct AppState {
    pub snapshot: Mutex<Snapshot>,
    pub procs: Mutex<Vec<ProcInfo>>,
    /// Uso de GPU en % × 10 (i32 evita f64 atómico): -10 = sin dato.
    pub gpu_util: AtomicI32,
    pub temp: Mutex<TempState>,
    pub gpu_info: Mutex<GpuInfoCache>,
    pub pinned: AtomicBool,
    pub mode: Mutex<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(Snapshot {
                ok: true,
                temp_status: "none".into(),
                gpu: -1.0,
                ..Default::default()
            }),
            procs: Mutex::new(Vec::new()),
            gpu_util: AtomicI32::new(-10),
            temp: Mutex::new(TempState { celsius: -1.0, status: "none".into() }),
            gpu_info: Mutex::new(GpuInfoCache {
                at: Instant::now() - Duration::from_secs(600),
                info: GpuInfo::default(),
            }),
            pinned: AtomicBool::new(true),
            mode: Mutex::new("dev".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Utilidades
// ---------------------------------------------------------------------------
fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

fn round2(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// Evita que un PID malicioso derrame bytes nulos en el nombre del proceso.
fn sanitize(s: &str) -> String {
    s.replace(['\0', '\r', '\n'], " ").trim().to_string()
}

fn toggle_widget(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}

// ---------------------------------------------------------------------------
// Comandos IPC (misma superficie que Electron: getSystemStats, getTopProcesses,
// getGpuInfo, killProcess, toggleAlwaysOnTop, getAlwaysOnTop, setWidgetMode).
// ---------------------------------------------------------------------------
#[tauri::command]
fn get_system_stats(state: State<'_, Arc<AppState>>) -> Value {
    serde_json::to_value(&*state.snapshot.lock().unwrap()).unwrap_or(json!({ "ok": false }))
}

#[tauri::command]
fn get_top_processes(state: State<'_, Arc<AppState>>) -> Value {
    json!({ "ok": true, "processes": *state.procs.lock().unwrap() })
}

const GPU_INFO_TTL: Duration = Duration::from_secs(10 * 60);

/// Async: la consulta WMI tarda ~1-2 s y no debe congelar el hilo principal.
/// (Los comandos async con referencias deben devolver Result en Tauri.)
#[tauri::command]
async fn get_gpu_info(state: State<'_, Arc<AppState>>) -> Result<Value, ()> {
    let stale = {
        let cache = state.gpu_info.lock().unwrap();
        cache.at.elapsed() >= GPU_INFO_TTL
    };
    if stale {
        let info = tauri::async_runtime::spawn_blocking(sampler::read_gpu_info)
            .await
            .unwrap_or_default();
        let mut cache = state.gpu_info.lock().unwrap();
        cache.info = info;
        cache.at = Instant::now();
    }
    let cache = state.gpu_info.lock().unwrap();
    Ok(serde_json::to_value(&cache.info).unwrap_or(json!({ "ok": false })))
}

#[tauri::command]
fn kill_process(pid: i64) -> Value {
    // Validación equivalente a validatePid(): entero positivo y distinto del widget.
    if pid <= 0 || pid as u32 == std::process::id() {
        return json!({ "ok": false, "error": "Invalid PID" });
    }
    // Windows: taskkill /F /T corta árboles de proceso (process.kill no).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                // El PID muerto desaparece en el próximo refresh de procesos.
                json!({ "ok": true, "pid": pid })
            }
            Ok(o) => json!({
                "ok": false,
                "error": format!(
                    "Failed to kill process {}: {}",
                    pid,
                    String::from_utf8_lossy(&o.stderr).trim()
                )
            }),
            Err(e) => json!({ "ok": false, "error": format!("Failed to kill process {} ({e})", pid) }),
        }
    }
    #[cfg(not(windows))]
    {
        json!({ "ok": false, "error": "Unsupported platform" })
    }
}

#[tauri::command]
fn toggle_always_on_top(app: AppHandle, state: State<'_, Arc<AppState>>) -> Value {
    let new = !state.pinned.load(Ordering::SeqCst);
    let ok = app
        .get_webview_window("main")
        .map(|win| win.set_always_on_top(new).is_ok())
        .unwrap_or(false);
    state.pinned.store(new, Ordering::SeqCst);
    json!({ "ok": ok, "pinned": new })
}

#[tauri::command]
fn get_always_on_top(state: State<'_, Arc<AppState>>) -> Value {
    json!({ "ok": true, "pinned": state.pinned.load(Ordering::SeqCst) })
}

#[tauri::command]
fn set_widget_mode(mode: String, app: AppHandle, state: State<'_, Arc<AppState>>) -> Value {
    if !matches!(mode.as_str(), "dev" | "mini" | "charts" | "procs") {
        return json!({ "ok": false, "error": "Invalid mode" });
    }
    let (w, h) = if mode == "mini" { (WIDGET_W, MINI_H) } else { (WIDGET_W, WIDGET_H) };
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_size(LogicalSize::new(w, h));
    }
    *state.mode.lock().unwrap() = mode.clone();
    json!({ "ok": true, "mode": mode })
}

// ---------------------------------------------------------------------------
// Arranque de la app
// ---------------------------------------------------------------------------
fn main() {
    tauri::Builder::default()
        // Instancia única: un segundo lanzamiento enfoca la ventana existente.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_shortcuts(["Ctrl+Shift+M"])
                .expect("shortcut inválido")
                .with_handler(|app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle_widget(app);
                    }
                })
                .build(),
        )
        .manage(Arc::new(AppState::default()))
        .setup(|app| {
            let state = app.state::<Arc<AppState>>().inner().clone();
            sampler::spawn_all(state);

            build_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Cerrar la ventana solo la oculta: el widget sigue vivo en la bandeja.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_system_stats,
            get_top_processes,
            get_gpu_info,
            kill_process,
            toggle_always_on_top,
            get_always_on_top,
            set_widget_mode
        ])
        .run(tauri::generate_context!())
        .expect("error while running System Monitor Widget");
}

/// Bandeja del sistema: clic izquierdo alterna mostrar/ocultar; menú contextual
/// con Show/Hide y Quit (equivale a main.js createTray()).
fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show = MenuItemBuilder::with_id("toggle", "Show / Hide  (Ctrl+Shift+M)").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&show, &PredefinedMenuItem::separator(app)?, &quit])
        .build()?;

    let icon = app.default_window_icon().cloned().unwrap_or_else(|| {
        // Fallback: icono PNG de 16×16 embebido (misma imagen que Electron).
        tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))
            .expect("embedded tray icon")
    });

    TrayIconBuilder::new()
        .icon(icon)
        .tooltip("System Monitor Widget — Ctrl+Shift+M")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" => toggle_widget(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_widget(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}