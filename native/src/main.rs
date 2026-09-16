//! sysmon-native — Widget de monitoreo del sistema con UI 100% nativa.
//!
//! Sin WebView: Win32 (ventana frameless, bandeja, hotkey) + Direct2D/DirectWrite
//! para dibujar. El muestreo, el estado y los formateadores vienen del crate
//! sysmon-core — los MISMOS que usa la variante Tauri: cero lógica duplicada.
//!
//! Presupuesto de memoria objetivo: < 15 MB en UN proceso (vs ~350-400 MB del
//! WebView2 multi-proceso).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// El tray API devuelve BOOLs informativos; los errores reales son imposibles
// aquí (el icono ya se creó) y las llamadas son idempotentes.
#![allow(unused_must_use)]
// Campos reservados de la capa de dibujo (factory/dwrite/formatos extra):
// se conservan porque la UI crece sobre ellos y su costo es un puntero COM.
#![allow(dead_code)]

mod ui;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, ScreenToClient, GetDeviceCaps, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE, HOT_KEY_MODIFIERS, MOD_CONTROL,
    MOD_SHIFT, VK_M,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIF_MESSAGE, NIF_ICON, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIIF_WARNING, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use sysmon_core::kill;
use sysmon_core::settings::Settings;
use sysmon_core::{sampler, AppState};

// ---------------------------------------------------------------------------
// Constantes del mensaje-loop
// ---------------------------------------------------------------------------
const WM_TRAY: u32 = WM_APP + 1;
const ID_TRAY: u32 = 1;
const HOTKEY_ID: i32 = 1;
/// Temporizadores: métricas 2.5 s (igual que el renderer), sin timer de
/// dibujo: se repinta por eventos (tick, mouse, modo, click).
const TIMER_METRICS: usize = 1;
const METRICS_MS: u32 = 2500;

// ---------------------------------------------------------------------------
// Guardián proactivo (port del renderer): streaks + latch + cooldown, y en
// vez de Notification() HTML5 → globos de la bandeja.
// ---------------------------------------------------------------------------

const STREAK_NEEDED: u32 = 2;
const COOLDOWN: Duration = Duration::from_secs(60);

struct Guardian {
    cpu_streak: u32,
    temp_streak: u32,
    gpu_streak: u32,
    ram_streak: u32,
    cpu_fired: bool,
    temp_fired: bool,
    gpu_fired: bool,
    ram_fired: bool,
    last: [Option<Instant>; 4],
}

/// Snapshot de los umbrales vigentes (configurables desde Ajustes).
struct Th {
    cpu: f64,
    ram: f64,
    gpu: f64,
    temp: f64,
}

fn thresholds_of(app: &AppState) -> Th {
    let t = app.settings.lock().unwrap().thresholds;
    Th { cpu: t.cpu, ram: t.ram, gpu: t.gpu, temp: t.temp }
}

impl Default for Guardian {
    fn default() -> Self {
        // `last` arranca en None (canal nunca notificado) → ready() es true y
        // la 1ª alerta de cada tipo sale sin esperar. Sin aritmética de
        // Instant: nada que paniquee en máquinas con poco uptime.
        Self {
            cpu_streak: 0,
            temp_streak: 0,
            gpu_streak: 0,
            ram_streak: 0,
            cpu_fired: false,
            temp_fired: false,
            gpu_fired: false,
            ram_fired: false,
            last: [None; 4],
        }
    }
}

/// Snapshot plano de métricas+umbrales que consume `Guardian::evaluate`.
/// Desacopla la lógica de decisión del estado Win32/AppState: en tests se
/// construye a mano, sin Mutex ni ventana.
struct GuardianInput {
    cpu: f64,
    temp: f64,
    gpu: f64,
    ram: f64,
    used_gb: f64,
    total_gb: f64,
    th_cpu: f64,
    th_temp: f64,
    th_gpu: f64,
    th_ram: f64,
}

/// Índices del array `last` (Some = instante del último disparo): 0=cpu,
/// 1=temp, 2=gpu, 3=ram.
impl Guardian {
    /// Tick del guardián: lee métricas/umbrales del estado compartido y delega
    /// la decisión en `evaluate` (la parte pura y testeable). `hwnd` es None
    /// en tests; con Some dispara el globo de la bandeja.
    fn check(&mut self, app: &Arc<AppState>, hwnd: Option<HWND>) {
        let snap = app.snapshot.lock().unwrap();
        let th = thresholds_of(app);
        let m = GuardianInput {
            cpu: snap.cpu.clamp(0.0, 100.0),
            temp: snap.cpu_temp,
            gpu: snap.gpu,
            ram: snap.memory.percent.clamp(0.0, 100.0),
            used_gb: snap.memory.used_gb,
            total_gb: snap.memory.total_gb,
            th_cpu: th.cpu,
            th_temp: th.temp,
            th_gpu: th.gpu,
            th_ram: th.ram,
        };
        drop(snap);
        self.evaluate(&m, &mut |title, body| {
            if let Some(h) = hwnd {
                unsafe { tray_balloon(h, title, body) }
            }
        });
    }

    /// Lógica PURA del guardián (testeable sin ventana ni Win32). Reglas por
    /// canal (cpu/temp/gpu/ram):
    ///  - Sentinela negativa (temp/gpu = -1: sin datos) → nunca alerta.
    ///  - Sobre el umbral → streak +1; al llegar a STREAK_NEEDED, sin latch
    ///    activo y con cooldown vencido → dispara UNA vez y marca el instante.
    ///  - Bajo el umbral → streak y latch se resetean (episodio terminado).
    fn evaluate(&mut self, m: &GuardianInput, notify: &mut dyn FnMut(&str, &str)) {
        // CPU
        if m.cpu > m.th_cpu {
            self.cpu_streak += 1;
            if self.cpu_streak >= STREAK_NEEDED && !self.cpu_fired && self.ready(0) {
                self.cpu_fired = true;
                self.mark(0);
                notify(
                    "\u{26A0}\u{FE0F} CPU Alert",
                    &format!("CPU {:.1}% sostenida sobre {}%", m.cpu, m.th_cpu),
                );
            }
        } else {
            self.cpu_streak = 0;
            self.cpu_fired = false;
        }

        // TEMP (-1 = sin sensor: nunca alerta).
        if m.temp > 0.0 && m.temp >= m.th_temp {
            self.temp_streak += 1;
            if self.temp_streak >= STREAK_NEEDED && !self.temp_fired && self.ready(1) {
                self.temp_fired = true;
                self.mark(1);
                notify(
                    "\u{1F321}\u{FE0F} Thermal Alert",
                    &format!("CPU {}°C sostenida sobre {}°C", m.temp as u32, m.th_temp),
                );
            }
        } else {
            self.temp_streak = 0;
            self.temp_fired = false;
        }

        // GPU (-1 = contadores aún no disponibles: nunca alerta).
        if m.gpu > 0.0 && m.gpu >= m.th_gpu {
            self.gpu_streak += 1;
            if self.gpu_streak >= STREAK_NEEDED && !self.gpu_fired && self.ready(2) {
                self.gpu_fired = true;
                self.mark(2);
                notify(
                    "\u{1F3AE} GPU Alert",
                    &format!("GPU {:.1}% sostenida sobre {}%", m.gpu, m.th_gpu),
                );
            }
        } else {
            self.gpu_streak = 0;
            self.gpu_fired = false;
        }

        // RAM
        if m.ram >= m.th_ram {
            self.ram_streak += 1;
            if self.ram_streak >= STREAK_NEEDED && !self.ram_fired && self.ready(3) {
                self.ram_fired = true;
                self.mark(3);
                notify(
                    "\u{1F9E0} RAM Alert",
                    &format!(
                        "Memoria {:.1}% sostenida sobre {}% ({:.1}/{:.1} GB)",
                        m.ram, m.th_ram, m.used_gb, m.total_gb
                    ),
                );
            }
        } else {
            self.ram_streak = 0;
            self.ram_fired = false;
        }
    }

    fn ready(&self, i: usize) -> bool {
        // None = canal sin disparar nunca → cooldown vencido.
        self.last[i].map_or(true, |t| t.elapsed() >= COOLDOWN)
    }
    fn mark(&mut self, i: usize) {
        self.last[i] = Some(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// Estado de la app (guardado en GWLP_USERDATA)
// ---------------------------------------------------------------------------
struct App {
    state: Arc<AppState>,
    ui: ui::UiState,
    gfx: Option<ui::Gfx>,
    scale: f32,
    mouse: (f32, f32), // coords lógicas
    tracking: bool,
    guardian: Guardian,
}

impl App {
    /// Empuja las métricas al historial de gráficas (port de pollTick):
    /// sentinelas → NaN (hueco honesto en las gráficas, nunca cero falso).
    fn push_metrics(&mut self) {
        let snap = self.state.snapshot.lock().unwrap().clone();
        let net_kib = (snap.network.rx_bytes_sec + snap.network.tx_bytes_sec) / 1024.0;
        let temp = if snap.cpu_temp > -1.0 { snap.cpu_temp } else { f64::NAN };
        let gpu = if snap.gpu > -1.0 { snap.gpu } else { f64::NAN };
        let dr = if snap.disk.read_bytes_sec >= 0.0 {
            snap.disk.read_bytes_sec / 1024.0
        } else {
            f64::NAN
        };
        let dw = if snap.disk.write_bytes_sec >= 0.0 {
            snap.disk.write_bytes_sec / 1024.0
        } else {
            f64::NAN
        };
        self.ui.history.push(
            snap.cpu.clamp(0.0, 100.0),
            temp,
            snap.memory.percent.clamp(0.0, 100.0),
            gpu,
            net_kib,
            dr,
            dw,
        );
    }
}

// ---------------------------------------------------------------------------
// Bandeja (tray): icono + tooltip + globos de alerta
// ---------------------------------------------------------------------------
fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = ID_TRAY;
    nid.uFlags = NIF_MESSAGE;
    nid.uCallbackMessage = WM_TRAY;
    nid
}

#[allow(unused_must_use)]
unsafe fn tray_add(hwnd: HWND) {
    let mut nid = tray_data(hwnd);
    let icon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
    nid.hIcon = icon;
    // NIF_ICON + NIF_TIP: sin ellos Windows ignora hIcon y szTip → icono
    // genérico en blanco y tooltip vacío (bug de presentación detectado).
    nid.uFlags |= NIF_ICON | NIF_TIP;
    let tip = "System Monitor Widget — Ctrl+Shift+M";
    let chars: Vec<u16> = tip.encode_utf16().take(127).collect();
    nid.szTip[..chars.len()].copy_from_slice(&chars);
    nid.szTip[chars.len().min(127)] = 0;
    Shell_NotifyIconW(NIM_ADD, &nid);
}

#[allow(unused_must_use)]
unsafe fn tray_del(hwnd: HWND) {
    let nid = tray_data(hwnd);
    Shell_NotifyIconW(NIM_DELETE, &nid);
}

/// Globo de notificación (equivalente nativo de Notification() del renderer).
#[allow(unused_must_use)]
unsafe fn tray_balloon(hwnd: HWND, title: &str, body: &str) {
    let mut nid = tray_data(hwnd);
    nid.uFlags |= NIF_INFO;
    let t: Vec<u16> = title.encode_utf16().take(63).collect();
    nid.szInfoTitle[..t.len()].copy_from_slice(&t);
    nid.szInfoTitle[t.len().min(63)] = 0;
    let b: Vec<u16> = body.encode_utf16().take(255).collect();
    nid.szInfo[..b.len()].copy_from_slice(&b);
    nid.szInfo[b.len().min(255)] = 0;
    nid.dwInfoFlags = NIIF_WARNING | NOTIFY_ICON_INFOTIP_FLAGS(0);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    let _ = NOTIFY_ICON_DATA_FLAGS(0);
}

// ---------------------------------------------------------------------------
// Helpers de ventana
// ---------------------------------------------------------------------------
fn logical_to_physical(v: f64, scale: f32) -> i32 {
    (v * scale as f64).round() as i32
}

unsafe fn set_visible(app: &mut App, hwnd: HWND, visible: bool) {
    app.state.visible.store(visible, Ordering::SeqCst);
    if visible {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
        app.push_metrics();
        InvalidateRect(Some(hwnd), None, false);
    } else {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// Cambia de modo: redimensiona la ventana (el backend es la fuente de verdad,
/// igual que set_widget_mode en la variante Tauri) y PERSISTE el modo.
unsafe fn set_mode(app: &mut App, hwnd: HWND, mode: &'static str) {
    if app.ui.mode == mode {
        return;
    }
    app.ui.mode = mode;
    *app.state.mode.lock().unwrap() = mode.to_string();
    {
        let mut st = app.state.settings.lock().unwrap();
        st.mode = mode.to_string();
        let _ = st.save();
    }
    let (w, h) = ui::mode_size(mode);
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        logical_to_physical(w, app.scale),
        logical_to_physical(h, app.scale),
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
    if mode == "dev" || mode == "procs" {
        // La lista pudo quedar desactualizada en otros modos: igual que el renderer.
        app.push_metrics();
    }
    InvalidateRect(Some(hwnd), None, false);
}

/// Guarda el modo/pin/posición vigentes (un solo JSON atómico).
fn persist(app: &App) {
    let mut st = app.state.settings.lock().unwrap();
    st.mode = app.ui.mode.to_string();
    st.pinned = app.state.pinned.load(Ordering::SeqCst);
    let _ = st.save();
}

/// Posición actual de la ventana en píxeles físicos de pantalla.
unsafe fn window_pos(hwnd: HWND) -> (i32, i32) {
    // GetWindowRect da directamente el origen de la ventana en coords de pantalla.
    let mut wr = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut wr);
    (wr.left, wr.top)
}

/// Snap del borde: si la ventana quedó a ≤ 16 px de un borde de la pantalla
/// primaria, se imanta a él. Devuelve la posición final.
unsafe fn snap_edge(hwnd: HWND) -> (i32, i32) {
    let mut wr = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut wr);
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    const SNAP: i32 = 16;
    let mut x = wr.left;
    let mut y = wr.top;
    if x.abs() <= SNAP {
        x = 0;
    }
    if (sw - wr.right).abs() <= SNAP {
        x = sw - (wr.right - wr.left);
    }
    if y.abs() <= SNAP {
        y = 0;
    }
    if (sh - wr.bottom).abs() <= SNAP {
        y = sh - (wr.bottom - wr.top);
    }
    (x, y)
}

/// Hit-test de los controles dibujados (coords lógicas).
fn hit<'a>(ui: &'a ui::UiState, x: f32, y: f32) -> Option<ui::Action> {
    ui.hits.iter().find(|h| h.rect.contains(x, y)).map(|h| h.action)
}

// ---------------------------------------------------------------------------
// WndProc
// ---------------------------------------------------------------------------
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // WM_NCHITTEST no necesita estado: header arrastrable siempre que exista
    // la ventana (con userdata nulo, DefWindowProc responde).
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;

    match msg {
        WM_ERASEBKGND => return LRESULT(1), // sin flicker: pintamos todo en WM_PAINT
        WM_NCHITTEST => {
            // El header es zona de arrastre nativa (equivale a
            // data-tauri-drag-region): arrastrar el widget por su cabecera.
            // Con el overlay de Ajustes abierto, la zona bajo la cabecera
            // vuelve a ser HTCLIENT: los botones del overlay deben recibir
            // el clic (HTCAPTION se los tragaba → overlay no clicable).
            let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let mut pt = POINT { x: ((lparam.0) & 0xFFFF) as u16 as i16 as i32, y };
            let _ = ScreenToClient(hwnd, &mut pt);
            if let Some(app) = ptr.as_ref() {
                if (pt.y as f32) < 36.0 * app.scale && !app.ui.show_settings {
                    return LRESULT(HTCAPTION as isize);
                }
            }
            return LRESULT(HTCLIENT as isize);
        }
        _ => {}
    }

    let Some(app) = ptr.as_mut() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let _ = hdc;
            if let Some(gfx) = &app.gfx {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let (w, h) = (rc.right as f32, rc.bottom as f32);
                let mut ctx = ui::DrawCtx {
                    app: &app.state,
                    ui: &mut app.ui,
                    scale: app.scale,
                    w,
                    h,
                };
                ui::draw(gfx, &mut ctx);
                ui::draw_tooltip(gfx, &ctx, app.mouse);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(gfx) = &app.gfx {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let _ = gfx.resize(rc.right.max(1) as u32, rc.bottom.max(1) as u32);
            }
            InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_TIMER => match wparam.0 {
            TIMER_METRICS => {
                if app.state.visible.load(Ordering::SeqCst) {
                    app.push_metrics();
                    app.guardian.check(&app.state, Some(hwnd));
                    InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            _ => LRESULT(0),
        },
        WM_HOTKEY => {
            let will_show = !app.state.visible.load(Ordering::SeqCst);
            set_visible(app, hwnd, will_show);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = ((lparam.0) & 0xFFFF) as u16 as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            app.mouse = (x as f32 / app.scale, y as f32 / app.scale);
            if !app.tracking {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if TrackMouseEvent(&mut tme).is_ok() {
                    app.tracking = true;
                }
            }
            // Hover por fila (tooltip del Top-5).
            let new_row = app
                .ui
                .rows
                .iter()
                .position(|r| r.contains(app.mouse.0, app.mouse.1));
            let changed = new_row != app.ui.hover_row
                || hover_changed(&app.ui, app.mouse);
            app.ui.hover_row = new_row;
            if changed {
                InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            app.tracking = false;
            app.ui.hover_row = None;
            app.ui.hover_btn = None;
            InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            app.mouse = (
                ((lparam.0) & 0xFFFF) as u16 as i16 as f32 / app.scale,
                ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32 / app.scale,
            );
            if let Some(action) = hit(&app.ui, app.mouse.0, app.mouse.1) {
                handle_action(app, hwnd, action);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Cerrar = ocultar a bandeja (la app sigue viva), igual que
            // CloseRequested → prevent_close + hide en la variante Tauri.
            set_visible(app, hwnd, false);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            // Al soltar la ventana (drag terminado): imantar a los bordes
            // cercanos y persistir la posición para el próximo arranque.
            let (x, y) = snap_edge(hwnd);
            if (x, y) != window_pos(hwnd) {
                let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            let (x, y) = window_pos(hwnd);
            {
                let mut st = app.state.settings.lock().unwrap();
                st.x = Some(x);
                st.y = Some(y);
                let _ = st.save();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            tray_del(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        // Mensaje del icono de bandeja.
        _ if msg == WM_TRAY => {
            match (lparam.0 & 0xFFFF) as u16 as u32 {
                WM_LBUTTONUP => {
                    let will_show = !app.state.visible.load(Ordering::SeqCst);
                    set_visible(app, hwnd, will_show);
                }
                WM_RBUTTONUP => tray_menu(app, hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn hover_changed(ui: &ui::UiState, mouse: (f32, f32)) -> bool {
    let new_btn = ui.hits.iter().find(|h| h.rect.contains(mouse.0, mouse.1)).map(|h| h.action);
    new_btn != ui.hover_btn
}

/// Menú contextual de la bandeja: Mostrar/Ocultar y Salir.
unsafe fn tray_menu(app: &mut App, hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, 1, w!("Mostrar / Ocultar  (Ctrl+Shift+M)"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, w!(""));
    let _ = AppendMenuW(menu, MF_STRING, 2, w!("Salir"));
    let _ = SetForegroundWindow(hwnd);
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    match cmd.0 {
        1 => {
            let will_show = !app.state.visible.load(Ordering::SeqCst);
            set_visible(app, hwnd, will_show);
        }
        2 => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

/// Ejecuta la acción de un control golpeado.
unsafe fn handle_action(app: &mut App, hwnd: HWND, action: ui::Action) {
    match action {
        ui::Action::Close => set_visible(app, hwnd, false),
        ui::Action::Pin => {
            let new = !app.state.pinned.load(Ordering::SeqCst);
            let after = if new { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            app.state.pinned.store(new, Ordering::SeqCst);
            app.ui.pinned = new;
            InvalidateRect(Some(hwnd), None, false);
        }
        ui::Action::Mode(m) => set_mode(app, hwnd, m),
        ui::Action::Expand => set_mode(app, hwnd, "dev"),
        ui::Action::Kill(pid) => {
            let text = w!("¿Terminar este proceso?\nSe usará taskkill /F /T (fuerza + árbol).");
            let caption = w!("Confirmar Kill");
            let res = MessageBoxW(
                Some(hwnd),
                text,
                caption,
                MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND,
            );
            if res == IDYES {
                // El error (permisos, PID ya muerto) se reporta en un globo.
                match kill::kill_process(Some(pid as i64)) {
                    Ok(_) => {}
                    Err(e) => tray_balloon(hwnd, "Kill falló", &e),
                }
                InvalidateRect(Some(hwnd), None, false);
            }
        }
        ui::Action::ToggleSettings => {
            app.ui.show_settings = !app.ui.show_settings;
            InvalidateRect(Some(hwnd), None, false);
        }
        ui::Action::Th(field, delta) => {
            {
                let mut st = app.state.settings.lock().unwrap();
                let cur = match field {
                    "cpu" => Some(st.thresholds.cpu),
                    "ram" => Some(st.thresholds.ram),
                    "gpu" => Some(st.thresholds.gpu),
                    "temp" => Some(st.thresholds.temp),
                    _ => None,
                };
                if let Some(v) = cur {
                    st.thresholds.set(field, v + delta as f64);
                    let _ = st.save();
                }
            }
            InvalidateRect(Some(hwnd), None, false);
        }
    }
}

// ---------------------------------------------------------------------------
// Hilo de metadatos de GPU (TTL 10 min — igual que get_gpu_info en Tauri)
// ---------------------------------------------------------------------------
fn spawn_gpu_info(state: Arc<AppState>) {
    std::thread::spawn(move || loop {
        let info = sampler::read_gpu_info();
        {
            let mut cache = state.gpu_info.lock().unwrap();
            cache.info = info;
            cache.at = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(600));
    });
}

// ---------------------------------------------------------------------------
// main: registrar clase, crear ventana, arrancar samplers y bucle de mensajes
// ---------------------------------------------------------------------------
fn main() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hinstance = GetModuleHandleW(None)?;
        let class_name = w!("SysMonNativeWidget");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: Default::default(),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Estado compartido (el MISMO que la variante Tauri), con las
        // preferencias persistidas cargadas al arranque (modo, pin, umbrales,
        // posición). Cargar NUNCA falla: dispares → defaults.
        let loaded = Settings::load();
        let start_mode: &'static str = match loaded.mode.as_str() {
            "mini" => "mini",
            "charts" => "charts",
            "procs" => "procs",
            _ => "dev",
        };
        let start_pinned = loaded.pinned;
        let (saved_x, saved_y) = (loaded.x, loaded.y);
        let state = Arc::new(AppState::default());
        *state.settings.lock().unwrap() = loaded;
        sampler::spawn_all(state.clone());
        spawn_gpu_info(state.clone());

        let scale = {
            // DPI del monitor primario antes de crear la ventana: 96 → 1.0.
            let hdc = windows::Win32::Graphics::Gdi::GetDC(None);
            let dpi = GetDeviceCaps(Some(hdc), windows::Win32::Graphics::Gdi::LOGPIXELSX);
            let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, hdc);
            dpi as f32 / 96.0
        };

        let app = Box::new(App {
            state,
            ui: ui::UiState::default(),
            gfx: None,
            scale,
            mouse: (0.0, 0.0),
            tracking: false,
            guardian: Guardian::default(),
        });

        // Ventana frameless: posición guardada o esquina superior derecha.
        let cx = GetSystemMetrics(SM_CXSCREEN);
        let (lw, lh) = ui::mode_size(start_mode);
        let (pw, ph) = (logical_to_physical(lw, scale), logical_to_physical(lh, scale));
        let (px, py) = match (saved_x, saved_y) {
            (Some(x), Some(y)) => (x, y),
            _ => (cx - pw - 24, 96),
        };
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | if start_pinned { WS_EX_TOPMOST } else { WINDOW_EX_STYLE(0) },
            class_name,
            w!("System Monitor Widget"),
            WS_POPUP,
            px,
            py,
            pw,
            ph,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

        // Volver a tomar el puntero para configurar lo que necesita el HWND.
        let app = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App);
        app.gfx = Some(ui::create_gfx(hwnd, pw as u32, ph as u32)?);
        app.ui.mode = start_mode;
        app.ui.pinned = start_pinned;
        *app.state.mode.lock().unwrap() = start_mode.to_string();
        app.state.pinned.store(start_pinned, Ordering::SeqCst);
        app.state.visible.store(true, Ordering::SeqCst);
        if !start_pinned {
            // El topmost es parte del estilo: sin pin, quitarlo tras crear.
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        // Temporizadores + hotkey + bandeja.
        SetTimer(Some(hwnd), TIMER_METRICS, METRICS_MS, None);
        // El hotkey global es un extra, no algo vital: si otro proceso ya lo
        // registró (p. ej. la variante Tauri en la misma sesión), la app debe
        // arrancar igual y sin él. Antes el `?` mataba todo el proceso aquí.
        if RegisterHotKey(Some(hwnd), HOTKEY_ID, HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_SHIFT.0), VK_M.0 as u32).is_err() {
            eprintln!("sysmon-native: Ctrl+Shift+M ocupado por otro proceso; continuando sin hotkey");
        }
        tray_add(hwnd);

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        // Bucle de mensajes.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg); 
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests del guardián: la decisión vive en `evaluate` (pura), así que streaks,
// latch y cooldown se prueban sin ventana ni estado Win32.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod guardian_tests {
    use super::*;

    /// Snapshot de prueba: umbral CPU configurable, resto alto/quieto.
    fn input(cpu: f64) -> GuardianInput {
        GuardianInput {
            cpu,
            temp: -1.0,   // sin sensor: canal TEMP mudo
            gpu: -1.0,    // sin contadores: canal GPU mudo
            ram: 40.0,
            used_gb: 8.0,
            total_gb: 16.0,
            th_cpu: 85.0,
            th_temp: 80.0,
            th_gpu: 90.0,
            th_ram: 90.0,
        }
    }

    fn tick(g: &mut Guardian, m: &GuardianInput) -> Vec<String> {
        let mut fired: Vec<String> = Vec::new();
        g.evaluate(m, &mut |title, _body| fired.push(title.to_string()));
        fired
    }

    #[test]
    fn no_dispara_si_nunca_supera_el_umbral() {
        let mut g = Guardian::default();
        for _ in 0..10 {
            assert!(tick(&mut g, &input(84.9)).is_empty());
        }
    }

    #[test]
    fn umbral_es_estricto_cpu_mayor_no_mayor_igual() {
        let mut g = Guardian::default();
        // CPU == umbral NO cuenta (la comparación es >).
        assert!(tick(&mut g, &input(85.0)).is_empty());
        assert!(tick(&mut g, &input(85.0)).is_empty());
        // 85.1 ya supera: primer tick del streak.
        assert!(tick(&mut g, &input(85.1)).is_empty());
        assert_eq!(tick(&mut g, &input(85.1)).len(), 1);
    }

    #[test]
    fn streak_necesita_2_ticks_consecutivos() {
        let mut g = Guardian::default();
        // Un tick aislado sobre umbral → nada (anti-falsas alarmas).
        assert!(tick(&mut g, &input(99.0)).is_empty());
        // Baja un tick → streak reseteado.
        assert!(tick(&mut g, &input(50.0)).is_empty());
        // Vuelve a subir: el streak empieza de cero.
        assert!(tick(&mut g, &input(99.0)).is_empty());
        assert_eq!(tick(&mut g, &input(99.0)).len(), 1);
    }

    #[test]
    fn latch_dispara_una_sola_vez_por_episodio() {
        let mut g = Guardian::default();
        assert!(tick(&mut g, &input(99.0)).is_empty());
        let first = tick(&mut g, &input(99.0));
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("CPU"));
        // Sigue alto: sin más alertas (latch activo).
        for _ in 0..20 {
            assert!(tick(&mut g, &input(99.0)).is_empty());
        }
    }

    #[test]
    fn cooldown_permite_renotificar_tras_episodio_separado() {
        let mut g = Guardian::default();
        // Episodio 1: dispara en el 2º tick y se apaga.
        tick(&mut g, &input(99.0));
        tick(&mut g, &input(99.0));
        // Baja → resetea streak y latch.
        for _ in 0..5 {
            tick(&mut g, &input(30.0));
        }
        // Episodio 2 inmediato: cooldown de 60 s NO vencido → no dispara.
        tick(&mut g, &input(99.0));
        assert!(tick(&mut g, &input(99.0)).is_empty());
        // Otra caída/subida tampoco dispara hasta vencer el cooldown.
        tick(&mut g, &input(30.0));
        tick(&mut g, &input(99.0));
        tick(&mut g, &input(99.0));
        assert!(tick(&mut g, &input(99.0)).is_empty());
        // La única vía de renotificación: baja, espera el cooldown y repite.
        tick(&mut g, &input(30.0));
        g.last[0] = Some(Instant::now() - COOLDOWN - Duration::from_secs(1));
        tick(&mut g, &input(99.0));
        assert_eq!(tick(&mut g, &input(99.0)).len(), 1);
    }

    #[test]
    fn cooldown_vencido_renotifica_en_nuevo_episodio() {
        let mut g = Guardian::default();
        tick(&mut g, &input(99.0));
        tick(&mut g, &input(99.0));
        tick(&mut g, &input(30.0)); // fin del episodio → latch OFF
        // Simula que pasó el cooldown sin dormir 60 s en el test.
        g.last[0] = Some(Instant::now() - COOLDOWN - Duration::from_secs(1));
        tick(&mut g, &input(99.0));
        let fired = tick(&mut g, &input(99.0));
        assert_eq!(fired.len(), 1);
        assert!(fired[0].contains("CPU"));
    }

    #[test]
    fn sentinela_negativo_no_alerta_nunca_temp_y_gpu() {
        let mut g = Guardian::default();
        // temp = -1 (sin sensor) y gpu = -1 (sin contadores): jamás alertan,
        // ni siquiera "superando" umbrales negativos.
        let mut m = input(30.0);
        m.temp = -1.0;
        m.gpu = -1.0;
        m.th_temp = -10.0;
        m.th_gpu = -10.0;
        for _ in 0..10 {
            assert!(tick(&mut g, &m).is_empty());
        }
    }

    #[test]
    fn temp_y_gpu_disparan_con_datos_validos() {
        let mut g = Guardian::default();
        let mut m = input(30.0);
        m.temp = 95.0;
        m.gpu = 95.0;
        // TEMP y GPU usan >= (incluye el umbral), al contrario que CPU.
        assert!(tick(&mut g, &m).is_empty()); // streak 1 (temp y gpu)
        let fired = tick(&mut g, &m);
        assert_eq!(fired.len(), 2);
        assert!(fired.iter().any(|t| t.contains("Thermal")));
        assert!(fired.iter().any(|t| t.contains("GPU")));
    }

    #[test]
    fn cpu_y_ram_son_canales_independientes() {
        let mut g = Guardian::default();
        let mut m = input(99.0);
        m.ram = 95.0; // ambos sobre umbral (ram >= 90)
        assert!(tick(&mut g, &m).is_empty());
        let fired = tick(&mut g, &m);
        assert_eq!(fired.len(), 2);
        assert!(fired.iter().any(|t| t.contains("CPU")));
        assert!(fired.iter().any(|t| t.contains("RAM")));
        // El latch es por canal: bajamos solo CPU → solo RAM sigue en latch.
        let mut m2 = input(30.0);
        m2.ram = 95.0;
        for _ in 0..5 {
            assert!(tick(&mut g, &m2).is_empty());
        }
    }

    #[test]
    fn ram_umbral_incluyente_y_mensaje_con_gigas() {
        let mut g = Guardian::default();
        let mut m = input(30.0);
        m.ram = 90.0; // == umbral: RAM SÍ cuenta (>=)
        assert!(tick(&mut g, &m).is_empty());
        let mut bodies: Vec<String> = Vec::new();
        // Segundo tick capturando también el cuerpo del globo.
        g.evaluate(&m, &mut |_t, b| bodies.push(b.to_string()));
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains("8.0/16.0 GB"));
    }

    #[test]
    fn thresholds_of_lee_los_umbrales_configurables() {
        let app = AppState::default();
        let th = thresholds_of(&app);
        assert_eq!(th.cpu, 85.0);
        assert_eq!(th.ram, 90.0);
        assert_eq!(th.gpu, 90.0);
        assert_eq!(th.temp, 80.0);
        // Cambio en vivo vía settings (lo que hace la pestaña de Ajustes).
        app.settings.lock().unwrap().thresholds.set("cpu", 70.0);
        assert_eq!(thresholds_of(&app).cpu, 70.0);
    }

    #[test]
    fn default_arranca_con_latch_apagado_y_cooldown_vencido() {
        let g = Guardian::default();
        assert!(!g.cpu_fired && !g.temp_fired && !g.gpu_fired && !g.ram_fired);
        assert_eq!(g.cpu_streak, 0);
        // last en None → ready() true desde el primer momento.
        for i in 0..4 {
            assert!(g.ready(i), "canal {i} debería estar listo al arrancar");
        }
    }
}
