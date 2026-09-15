//! main.rs — Proceso principal del widget en Tauri (Rust).
//!
//! Funcionalidad:
//!  - Ventana frameless, always-on-top, 340×500 (mini 340×245).
//!  - Bandeja (Tray) + atajo global Ctrl+Shift+M para mostrar/ocultar.
//!  - Instancia única (segundo lanzamiento enfoca la ventana existente).
//!  - 7 comandos IPC consumidos por el renderer
//!    (src/*) vía src/tauri-bridge.js.
//!  - Muestreo en segundo plano: CPU/RAM/red (sysinfo), GPU (typeperf/PDH),
//!    temperatura CPU (WMI MSAcpi_ThermalZoneTemperature) con estado honesto
//!    de 3 niveles ('ok' | 'admin' | 'none') y metadatos de GPU (WMI).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// El núcleo compartido (tipos, estado, sampler y formateadores) vive en el
// crate sysmon-core: lo usan este binario (vía IPC al WebView) y el binario
// nativo (native/) sin duplicar una sola línea.
use sysmon_core::{round1, AppState, MINI_H, WIDGET_H, WIDGET_W};
use sysmon_core::sampler;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, Runtime, State, WindowEvent};
use tauri_plugin_global_shortcut::ShortcutState;

// ---------------------------------------------------------------------------
// Fuente de verdad de la visibilidad: actualiza el flag que leen los gating
// threads del sampler Y notifica al renderer.
///
/// El evento es imprescindible porque WebView2 NO propaga `document.hidden`
/// cuando la ventana anfitriona se oculta: sin él, el renderer seguía
/// haciendo IPC + DOM + Chart cada 2.5 s mientras el widget estaba en bandeja.
fn set_visible_state<R: Runtime>(app: &AppHandle<R>, visible: bool) {
    app.state::<Arc<AppState>>()
        .visible
        .store(visible, Ordering::SeqCst);
    let _ = app.emit("visibility-changed", visible);
}

fn toggle_widget<R: Runtime>(app: &AppHandle<R>) {
    // Estado-driven (no win.is_visible()): el flag es la única fuente de
    // verdad, todas las transiciones pasan por aquí, y así el toggle es
    // determinista incluso si la consulta del dispatcher se desincroniza.
    let will_show = !app
        .state::<Arc<AppState>>()
        .visible
        .load(Ordering::SeqCst);
    if let Some(win) = app.get_webview_window("main") {
        if will_show {
            let _ = win.show();
            let _ = win.set_focus();
        } else {
            let _ = win.hide();
        }
    }
    set_visible_state(app, will_show);
}

// ---------------------------------------------------------------------------
// Comandos IPC (getSystemStats, getTopProcesses,
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

/// Estadísticas de sesión (máximos y promedios desde el arranque). Cero IPC
/// extra en el camino caliente: el renderer las consulta solo al abrir el
/// modo Gráficas y después cada 2.5 s junto con el resto del snapshot.
#[tauri::command]
fn get_session_stats(state: State<'_, Arc<AppState>>) -> Value {
    let s = state.session.lock().unwrap();
    json!({
        "ok": true,
        "samples": s.samples,
        "cpu": { "max": round1(s.cpu_max), "avg": round1(s.cpu_avg()) },
        "mem": { "max": round1(s.mem_max), "avg": round1(s.mem_avg()) },
        "gpu": { "max": round1(s.gpu_max), "avg": round1(s.gpu_avg()) },
        "net": { "max": round1(s.net_max), "avg": round1(s.net_avg()) }
    })
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

/// `Option<i64>`: el puente (tauri-bridge.js) manda `null` para PIDs no
/// numéricos/inválidos. La validación y la ejecución (taskkill /F /T) viven
/// en sysmon_core::kill, compartidas con el binario nativo.
#[tauri::command]
fn kill_process(pid: Option<i64>) -> Value {
    match sysmon_core::kill::kill_process(pid) {
        Ok(pid) => json!({ "ok": true, "pid": pid }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

#[tauri::command]
fn toggle_always_on_top<R: Runtime>(app: AppHandle<R>, state: State<'_, Arc<AppState>>) -> Value {
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

/// `Option<String>` por la misma razón que kill_process: el puente manda
/// `null` para modos inválidos y el contrato es devolver `{ ok: false }`.
/// Genérico sobre `R: Runtime` para poder probarlo con el MockRuntime de
/// tauri::test sin GUI (el wrapper del macro infiere R en producción).
#[tauri::command]
fn set_widget_mode<R: Runtime>(
    mode: Option<String>,
    app: AppHandle<R>,
    state: State<'_, Arc<AppState>>,
) -> Value {
    let Some(mode) = mode.filter(|m| matches!(m.as_str(), "dev" | "mini" | "charts" | "procs"))
    else {
        return json!({ "ok": false, "error": "Invalid mode" });
    };
    // Dedup idempotente: re-fijar el modo vigente NO re-emite ni re-dimensiona.
    // Sin esto, el eco renderer→backend (el renderer que sigue al evento
    // re-invocando el comando) duplicaba cada emisión 'mode-changed'.
    {
        let mut current = state.mode.lock().unwrap();
        if *current == mode {
            return json!({ "ok": true, "mode": mode });
        }
        *current = mode.clone();
    }
    // Persistir el modo: al arrancar de nuevo, el widget reaparece en el
    // modo en que se dejó (lo aplica el renderer leyendo get_settings).
    {
        let mut st = state.settings.lock().unwrap();
        st.mode = mode.clone();
        let _ = st.save();
    }        let (w, h) = if mode == "mini" { (WIDGET_W, MINI_H) } else { (WIDGET_W, WIDGET_H) };
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_size(LogicalSize::new(w, h));
    }
    // El backend es la fuente de verdad del modo: una llamada IPC directa
    // (tests E2E, atajos futuros) debe reflejarse en la UI del renderer.
    // Sin esto, el renderer queda desincronizado y mini/charts/procs dejan
    // de renderizarse aunque la ventana cambie de tamaño.
    let _ = app.emit("mode-changed", mode.clone());
    json!({ "ok": true, "mode": mode })
}

/// Oculta la ventana a la bandeja SIN destruir el webview.
///
/// El botón ✕ del renderer DEBE invocar este comando y NUNCA `window.close()`:
/// wry responde a `window.close()` destruyendo el HWND de WebView2
/// (add_WindowCloseRequested → DestroyWindow) sin pasar por el handler de
/// `CloseRequested` de Tauri, así que `prevent_close()` + `hide()` nunca se
/// ejecutan y la ventana queda NEGRA y pegada hasta reiniciar la app.
#[tauri::command]
fn hide_widget<R: Runtime>(app: AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
    set_visible_state(&app, false);
}

/// Espejo de hide_widget para volver desde la bandeja (equivalente al clic
/// del tray / Ctrl+Shift+M, disponible también por IPC para tests).
#[tauri::command]
fn show_widget<R: Runtime>(app: AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
    set_visible_state(&app, true);
}

/// Preferencias persistidas completas (modo, pin, umbrales, posición).
#[tauri::command]
fn get_settings(state: State<'_, Arc<AppState>>) -> Value {
    let st = state.settings.lock().unwrap().clone();
    json!({ "ok": true, "settings": st })
}

/// Ajusta un umbral con valor ABSOLUTO (contrato de set_threshold) y emite
/// 'thresholds-changed' para que el guardián del renderer siga el cambio.
#[tauri::command]
fn set_threshold<R: Runtime>(
    app: AppHandle<R>,
    kind: Option<String>,
    value: Option<f64>,
    state: State<'_, Arc<AppState>>,
) -> Value {
    apply_threshold(app, kind, value, state, ThresholdChange::Absolute)
}

/// Ajusta un umbral con DESFASE (delta) sobre el valor vigente, con clamp.
/// Es la variante que usan los steppers del overlay de Ajustes: manda −5/+5
/// y el backend resuelve el valor final; así nunca se pisa un cambio hecho
/// en otra sesión y el clamp es consistente.
#[tauri::command]
fn set_threshold_delta<R: Runtime>(
    app: AppHandle<R>,
    kind: Option<String>,
    delta: Option<f64>,
    state: State<'_, Arc<AppState>>,
) -> Value {
    apply_threshold(app, kind, delta, state, ThresholdChange::Delta)
}

/// Modo de ajuste de umbral: valor absoluto o desfase sobre el vigente.
enum ThresholdChange {
    Absolute,
    Delta,
}

/// Núcleo compartido de set_threshold/set_threshold_delta: valida, aplica,
/// persiste y emite 'thresholds-changed' (el backend es la fuente de verdad).
fn apply_threshold<R: Runtime>(
    app: AppHandle<R>,
    kind: Option<String>,
    value: Option<f64>,
    state: State<'_, Arc<AppState>>,
    change: ThresholdChange,
) -> Value {
    let Some(kind) = kind.filter(|k| matches!(k.as_str(), "cpu" | "ram" | "gpu" | "temp")) else {
        return json!({ "ok": false, "error": "Invalid kind" });
    };
    let Some(value) = value.filter(|v| v.is_finite()) else {
        return json!({ "ok": false, "error": "Invalid value" });
    };
    let mut st = state.settings.lock().unwrap();
    let current = match kind.as_str() {
        "cpu" => st.thresholds.cpu,
        "ram" => st.thresholds.ram,
        "gpu" => st.thresholds.gpu,
        _ => st.thresholds.temp,
    };
    let target = match change {
        ThresholdChange::Absolute => value,
        ThresholdChange::Delta => current + value,
    };
    st.thresholds.set(&kind, target);
    let _ = st.save();
    // Un cambio directo por IPC (tests, atajos) o delta de steppers debe
    // reflejarse en el guardián del renderer sin recargar.
    let _ = app.emit("thresholds-changed", st.thresholds);
    json!({ "ok": true, "thresholds": st.thresholds })
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
                set_visible_state(app, true);
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

            // Preferencias persistidas (modo, pin, umbrales, posición): se
            // cargan al arranque. El modo lo aplica el renderer al leer
            // get_settings; pin y posición se restauran aquí.
            let loaded = sysmon_core::settings::Settings::load();
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_always_on_top(loaded.pinned);
                if let (Some(x), Some(y)) = (loaded.x, loaded.y) {
                    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
                }
            }
            state.pinned.store(loaded.pinned, Ordering::SeqCst);
            *state.settings.lock().unwrap() = loaded;

            sampler::spawn_all(state);

            build_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Cerrar la ventana solo la oculta: el widget sigue vivo en la bandeja.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
                set_visible_state(window.app_handle(), false);
            }
            // Arrastrar el widget persiste la posición (física, igual que la
            // que se restaura al arrancar) para que reaparezca donde lo dejaste.
            WindowEvent::Moved(phys) => {
                let state = window.app_handle().state::<Arc<AppState>>();
                let mut st = state.settings.lock().unwrap();
                st.x = Some(phys.x);
                st.y = Some(phys.y);
                let _ = st.save();
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_system_stats,
            get_top_processes,
            get_session_stats,
            get_gpu_info,
            kill_process,
            toggle_always_on_top,
            get_always_on_top,
            set_widget_mode,
            hide_widget,
            show_widget,
            get_settings,
            set_threshold,
            set_threshold_delta
        ])
        .run(tauri::generate_context!())
        .expect("error while running System Monitor Widget");
}

#[cfg(test)]
mod mode_changed_tests {
    use super::*;
    use std::sync::Mutex;
    use sysmon_core::{round2, sanitize, ProcInfo};
    use serde_json::json;
    use tauri::test::{mock_app, mock_builder, noop_assets, mock_context};
    use tauri::{Listener, Manager};

    /// App mock con el MISMO estado global que la app real (AppState por defecto
    /// = modo 'dev', visible). El contexto mock usa los assets noop: sin webview
    /// real, perfecto para verificar la emisión del evento.
    ///
    /// SYSMON_SETTINGS_DIR a un temporal: los tests de comandos PERSISTEN
    /// settings; sin aislamiento pisarían el settings real del usuario.
    fn test_app() -> tauri::App<tauri::test::MockRuntime> {
        let dir = std::env::temp_dir().join(format!("sysmon-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("SYSMON_SETTINGS_DIR", dir.join("settings.json"));
        mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app")
    }

    #[test]
    fn modo_valido_emite_mode_changed_y_actualiza_estado() {
        let app = test_app();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());

        let payload = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = payload.clone();
        app.listen("mode-changed", move |event| {
            sink.lock().unwrap().push(event.payload().to_string());
        });

        let res = set_widget_mode(
            Some("mini".into()),
            app.handle().clone(),
            app.state::<Arc<AppState>>(),
        );

        assert_eq!(res, json!({ "ok": true, "mode": "mini" }));
        assert_eq!(*state.mode.lock().unwrap(), "mini");
        assert_eq!(
            payload.lock().unwrap().as_slice(),
            ["\"mini\""],
            "el evento debe emitirse exactamente una vez con el modo como payload"
        );
        app.cleanup_before_exit();
    }

    #[test]
    fn modo_invalido_no_emite_ni_cambia_estado() {
        let app = test_app();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());

        let hits = Arc::new(Mutex::new(0usize));
        let sink = hits.clone();
        app.listen("mode-changed", move |_| {
            *sink.lock().unwrap() += 1;
        });

        // null como el puente manda para modos inválidos (kill_process-style).
        let res = set_widget_mode(None, app.handle().clone(), app.state::<Arc<AppState>>());
        assert_eq!(res, json!({ "ok": false, "error": "Invalid mode" }));

        // Un modo fuera de la whitelist tampoco emite.
        let res = set_widget_mode(
            Some("fullscreen".into()),
            app.handle().clone(),
            app.state::<Arc<AppState>>(),
        );
        assert_eq!(res, json!({ "ok": false, "error": "Invalid mode" }));

        assert_eq!(*hits.lock().unwrap(), 0, "ningún evento para modos inválidos");
        assert_eq!(*state.mode.lock().unwrap(), "dev", "el modo no cambia");
        app.cleanup_before_exit();
    }

    #[test]
    fn cada_modo_valido_emite_su_propio_payload() {
        let app = test_app();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());

        let payload = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = payload.clone();
        app.listen("mode-changed", move |event| {
            sink.lock().unwrap().push(event.payload().to_string());
        });

        for modo in ["mini", "charts", "procs", "dev"] {
            let res = set_widget_mode(
                Some(modo.into()),
                app.handle().clone(),
                app.state::<Arc<AppState>>(),
            );
            assert_eq!(res, json!({ "ok": true, "mode": modo }));
        }

        assert_eq!(
            payload.lock().unwrap().as_slice(),
            ["\"mini\"", "\"charts\"", "\"procs\"", "\"dev\""],
            "un evento por cambio, en orden"
        );
        app.cleanup_before_exit();
    }

    #[test]
    fn get_settings_devuelve_estado_y_set_threshold_clampea() {
        let app = test_app();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());
        let handle = app.handle().clone();

        // set_threshold con kind inválido → ok:false
        let bad = set_threshold(handle.clone(), Some("nope".into()), Some(50.0), app.state::<Arc<AppState>>());
        assert_eq!(bad["ok"], json!(false));

        // set_threshold con valor no finito → ok:false
        let nan = set_threshold(handle.clone(), Some("cpu".into()), Some(f64::NAN), app.state::<Arc<AppState>>());
        assert_eq!(nan["ok"], json!(false));

        // Valor válido pero fuera de rango → clamped a [1,100]
        let clamped = set_threshold(handle.clone(), Some("cpu".into()), Some(5000.0), app.state::<Arc<AppState>>());
        assert_eq!(clamped["ok"], json!(true));
        assert_eq!(clamped["thresholds"]["cpu"], json!(100.0));

        // set_threshold_delta: +(-120) sobre 100 → clamp al mínimo (1.0)
        let delta = set_threshold_delta(handle.clone(), Some("cpu".into()), Some(-120.0), app.state::<Arc<AppState>>());
        assert_eq!(delta["ok"], json!(true));
        assert_eq!(delta["thresholds"]["cpu"], json!(1.0));

        // El estado global quedó actualizado y get_settings lo refleja
        let st = state.settings.lock().unwrap().clone();
        assert_eq!(st.thresholds.cpu, 1.0);
        let got = get_settings(app.state::<Arc<AppState>>());
        assert_eq!(got["settings"]["thresholds"]["cpu"], json!(1.0));
        assert!(got["settings"]["mode"].is_string());
    }

    #[test]
    fn mock_app_arranca_con_estado_por_defecto() {
        let app = mock_app();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());
        assert_eq!(*state.mode.lock().unwrap(), "dev");
        assert!(state.visible.load(Ordering::SeqCst));
        app.cleanup_before_exit();
    }
}

#[cfg(test)]
mod window_state_tests {
    use super::*;
    use std::sync::Mutex;
    use sysmon_core::{round2, sanitize, ProcInfo};
    use tauri::test::{mock_builder, mock_context, noop_assets};
    use tauri::{Listener, Manager, WebviewUrl, WebviewWindowBuilder};

    /// App mock con una ventana "main" real (dispatcher mock: hide/show son
    /// no-ops, así que lo verificable es el ESTADO visible del AppState y que
    /// los comandos no entran en pánico; la visibilidad Win32 real se cubre
    /// en el E2E con win-visibility.ps1).
    fn test_app_with_window() -> tauri::App<tauri::test::MockRuntime> {
        let app = mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app");
        WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
            .build()
            .expect("mock main window");
        app
    }

    #[test]
    fn hide_widget_marca_invisible_y_show_widget_lo_restaura() {
        let app = test_app_with_window();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());
        assert!(state.visible.load(Ordering::SeqCst), "arranca visible");

        hide_widget(app.handle().clone());
        assert!(!state.visible.load(Ordering::SeqCst), "hide → visible=false");

        show_widget(app.handle().clone());
        assert!(state.visible.load(Ordering::SeqCst), "show → visible=true");
        app.cleanup_before_exit();
    }

    #[test]
    fn transiciones_repetidas_son_idempotentes_y_coherentes() {
        let app = test_app_with_window();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());

        // Doble hide seguido de doble show: el flag debe acabar en true y las
        // repeticiones no deben corromper el estado (los gating threads leen
        // este flag cada 500 ms-2 s: un estado inconsistente congela métricas).
        hide_widget(app.handle().clone());
        hide_widget(app.handle().clone());
        assert!(!state.visible.load(Ordering::SeqCst));
        show_widget(app.handle().clone());
        show_widget(app.handle().clone());
        assert!(state.visible.load(Ordering::SeqCst));
        app.cleanup_before_exit();
    }

    #[test]
    fn comandos_no_entran_en_panico_sin_ventana() {
        // Robustez: si la ventana aún no existe (o ya murió), los guards
        // `if let Some(win)` deben degradar con gracia, no crashear.
        let app = mock_builder()
            .build(mock_context(noop_assets()))
            .expect("mock app");
        let state = Arc::new(AppState::default());
        app.manage(state.clone());

        hide_widget(app.handle().clone());
        assert!(!state.visible.load(Ordering::SeqCst), "estado cambia igual");
        show_widget(app.handle().clone());
        assert!(state.visible.load(Ordering::SeqCst));
        app.cleanup_before_exit();
    }

    #[test]
    fn toggle_always_on_top_invierte_y_reporta_el_estado() {
        let app = test_app_with_window();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());
        assert!(state.pinned.load(Ordering::SeqCst), "arranca fijado");

        let res = toggle_always_on_top(app.handle().clone(), app.state::<Arc<AppState>>());
        assert_eq!(res, json!({ "ok": true, "pinned": false }));
        assert!(!state.pinned.load(Ordering::SeqCst));

        let res = toggle_always_on_top(app.handle().clone(), app.state::<Arc<AppState>>());
        assert_eq!(res, json!({ "ok": true, "pinned": true }));

        let res = get_always_on_top(app.state::<Arc<AppState>>());
        assert_eq!(res, json!({ "ok": true, "pinned": true }));
        app.cleanup_before_exit();
    }

    #[test]
    fn kill_process_rechaza_pids_peligrosos_sin_ejecutar_nada() {
        // Sin ventana ni estado gestionado: estos caminos devuelven ANTES de
        // tocar taskkill, así que son seguros de probar en cualquier app.
        let res = kill_process(None); // lo que manda el puente con PID inválido
        assert_eq!(res, json!({ "ok": false, "error": "Invalid PID" }));

        let res = kill_process(Some(0));
        assert_eq!(res, json!({ "ok": false, "error": "Invalid PID" }));

        let res = kill_process(Some(-3));
        assert_eq!(res, json!({ "ok": false, "error": "Invalid PID" }));

        let self_pid = std::process::id() as i64;
        let res = kill_process(Some(self_pid));
        assert_eq!(
            res,
            json!({ "ok": false, "error": "Refusing to kill the widget itself" })
        );

        // Solo en Windows: PID 4 = System.
        #[cfg(windows)]
        {
            let res = kill_process(Some(4));
            assert_eq!(
                res,
                json!({ "ok": false, "error": "Refusing to kill a system-critical process" })
            );
        }
    }

    #[test]
    fn utilidades_sanitize_y_round() {
        // sanitize: bytes nulos/CR/LF de nombres de proceso maliciosos.
        assert_eq!(sanitize("explorer.exe"), "explorer.exe");
        assert_eq!(sanitize("bad\0name"), "bad name");
        assert_eq!(sanitize("weird\r\nname"), "weird  name");
        assert_eq!(sanitize("  padded  "), "padded");

        assert_eq!(round1(23.44), 23.4);
        assert_eq!(round1(23.45), 23.5); // .round() de Rust: half away from zero
        assert_eq!(round2(7.006), 7.01);
        assert_eq!(round1(-1.25), -1.3);
    }

    #[test]
    fn hide_y_show_emiten_visibility_changed_con_payload_bool() {
        let app = test_app_with_window();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());
        let payload = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = payload.clone();
        app.listen("visibility-changed", move |event| {
            sink.lock().unwrap().push(event.payload().to_string());
        });

        hide_widget(app.handle().clone());
        assert_eq!(*payload.lock().unwrap(), ["false".to_string()]);

        show_widget(app.handle().clone());
        assert_eq!(
            *payload.lock().unwrap(),
            ["false".to_string(), "true".to_string()]
        );
        app.cleanup_before_exit();
    }

    #[test]
    fn toggle_widget_emite_un_evento_por_transicion_real() {
        let app = test_app_with_window();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());
        let payload = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = payload.clone();
        app.listen("visibility-changed", move |event| {
            sink.lock().unwrap().push(event.payload().to_string());
        });

        // Arranca visible → el primer toggle oculta, el segundo muestra.
        toggle_widget(app.handle());
        toggle_widget(app.handle());
        assert_eq!(
            *payload.lock().unwrap(),
            ["false".to_string(), "true".to_string()]
        );
        assert!(state.visible.load(Ordering::SeqCst));
        app.cleanup_before_exit();
    }

    // ------------------- Ronda de datos: sesión + disco + exe -------------------

    #[test]
    fn get_session_stats_refleja_maximos_y_promedios_grabados() {
        let app = test_app_with_window();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());

        {
            let mut s = state.session.lock().unwrap();
            s.record(10.0, 40.0, 5.0, 1024.0);
            s.record(30.0, 60.0, -1.0, 2048.0); // GPU sin dato: no entra al promedio.
            s.record(20.0, 50.0, 25.0, 512.0);
        }

        let res = get_session_stats(app.state::<Arc<AppState>>());
        assert_eq!(res["ok"], json!(true));
        assert_eq!(res["samples"], json!(3));
        assert_eq!(res["cpu"]["max"], json!(30.0));
        assert_eq!(res["cpu"]["avg"], json!(20.0));
        assert_eq!(res["mem"]["max"], json!(60.0));
        assert_eq!(res["gpu"]["max"], json!(25.0));
        assert_eq!(res["gpu"]["avg"], json!(15.0)); // (5+25)/2, sin la muestra -1.
        assert_eq!(res["net"]["max"], json!(2048.0));
        app.cleanup_before_exit();
    }

    #[test]
    fn snapshot_inicial_y_session_stats_arrancan_en_neutro_honesto() {
        let app = test_app_with_window();
        let state = Arc::new(AppState::default());
        app.manage(state.clone());

        // Disco sin dato aún → sentinel -1 (la UI muestra 'n/a', nunca 0 falso).
        let snap = get_system_stats(app.state::<Arc<AppState>>());
        assert_eq!(snap["disk"]["readBytesSec"], json!(-1.0));
        assert_eq!(snap["disk"]["writeBytesSec"], json!(-1.0));

        // Sesión sin muestras → promedios 0 y samples 0 (sin división por cero).
        let res = get_session_stats(app.state::<Arc<AppState>>());
        assert_eq!(res["samples"], json!(0));
        assert_eq!(res["cpu"]["avg"], json!(0.0));
        assert_eq!(res["gpu"]["max"], json!(0.0));
        app.cleanup_before_exit();
    }

    #[test]
    fn proc_info_serializa_exe_y_toleran_null() {
        // Con ruta: el campo "exe" llega al renderer para el tooltip.
        let con_ruta = ProcInfo {
            pid: 42,
            name: "app.exe".into(),
            cpu: 1.5,
            mem: 2.25,
            exe: Some("C:\\Breiner\\app.exe".into()),
        };
        let v = serde_json::to_value(&con_ruta).unwrap();
        assert_eq!(v["exe"], json!("C:\\Breiner\\app.exe"));
        assert_eq!(v["pid"], json!(42));

        // Sin ruta (SO protegido): null explícito — el renderer degrada a
        // 'ruta no disponible' sin romperse.
        let sin_ruta = ProcInfo { exe: None, ..con_ruta };
        let v = serde_json::to_value(&sin_ruta).unwrap();
        assert!(v["exe"].is_null());
    }
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
        // Fallback: icono PNG de 16×16 embebido.
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