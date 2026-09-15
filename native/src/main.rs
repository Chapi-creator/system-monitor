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
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIF_MESSAGE, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIIF_WARNING,
    NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use sysmon_core::kill;
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
const TH_CPU: f64 = 85.0;
const TH_TEMP: f64 = 80.0;
const TH_GPU: f64 = 90.0;
const TH_RAM: f64 = 90.0;
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
    last: [Instant; 4],
}

impl Default for Guardian {
    fn default() -> Self {
        // Instant antiguo para que la 1ª notificación de cada tipo salga sin esperar.
        let old = Instant::now() - Duration::from_secs(3600);
        Self {
            cpu_streak: 0,
            temp_streak: 0,
            gpu_streak: 0,
            ram_streak: 0,
            cpu_fired: false,
            temp_fired: false,
            gpu_fired: false,
            ram_fired: false,
            last: [old; 4],
        }
    }
}

/// Índices del array `last`: 0=cpu, 1=temp, 2=gpu, 3=ram.
impl Guardian {
    fn check(&mut self, app: &Arc<AppState>, hwnd: HWND) {
        let snap = app.snapshot.lock().unwrap();
        let cpu = snap.cpu.clamp(0.0, 100.0);
        let temp = snap.cpu_temp;
        let gpu = snap.gpu;
        let ram = snap.memory.percent.clamp(0.0, 100.0);
        let used_gb = snap.memory.used_gb;
        let total_gb = snap.memory.total_gb;
        drop(snap);

        // CPU
        if cpu > TH_CPU {
            self.cpu_streak += 1;
            if self.cpu_streak >= STREAK_NEEDED && !self.cpu_fired && self.ready(0) {
                self.cpu_fired = true;
                self.mark(0);
                unsafe {
                    tray_balloon(
                        hwnd,
                        "\u{26A0}\u{FE0F} CPU Alert",
                        &format!("CPU {:.1}% sostenida sobre {}%", cpu, TH_CPU),
                    );
                }
            }
        } else {
            self.cpu_streak = 0;
            self.cpu_fired = false;
        }

        // TEMP (-1 = sin sensor: nunca alerta).
        if temp > 0.0 && temp >= TH_TEMP {
            self.temp_streak += 1;
            if self.temp_streak >= STREAK_NEEDED && !self.temp_fired && self.ready(1) {
                self.temp_fired = true;
                self.mark(1);
                unsafe {
                    tray_balloon(
                        hwnd,
                        "\u{1F321}\u{FE0F} Thermal Alert",
                        &format!("CPU {}°C sostenida sobre {}°C", temp as u32, TH_TEMP),
                    );
                }
            }
        } else {
            self.temp_streak = 0;
            self.temp_fired = false;
        }

        // GPU (-1 = contadores aún no disponibles: nunca alerta).
        if gpu > 0.0 && gpu >= TH_GPU {
            self.gpu_streak += 1;
            if self.gpu_streak >= STREAK_NEEDED && !self.gpu_fired && self.ready(2) {
                self.gpu_fired = true;
                self.mark(2);
                unsafe {
                    tray_balloon(
                        hwnd,
                        "\u{1F3AE} GPU Alert",
                        &format!("GPU {:.1}% sostenida sobre {}%", gpu, TH_GPU),
                    );
                }
            }
        } else {
            self.gpu_streak = 0;
            self.gpu_fired = false;
        }

        // RAM
        if ram >= TH_RAM {
            self.ram_streak += 1;
            if self.ram_streak >= STREAK_NEEDED && !self.ram_fired && self.ready(3) {
                self.ram_fired = true;
                self.mark(3);
                unsafe {
                    tray_balloon(
                        hwnd,
                        "\u{1F9E0} RAM Alert",
                        &format!(
                            "Memoria {:.1}% sostenida sobre {}% ({:.1}/{:.1} GB)",
                            ram, TH_RAM, used_gb, total_gb
                        ),
                    );
                }
            }
        } else {
            self.ram_streak = 0;
            self.ram_fired = false;
        }
    }

    fn ready(&self, i: usize) -> bool {
        self.last[i].elapsed() >= COOLDOWN
    }
    fn mark(&mut self, i: usize) {
        self.last[i] = Instant::now();
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
    nid.uFlags |= NIF_MESSAGE;
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
/// igual que set_widget_mode en la variante Tauri).
unsafe fn set_mode(app: &mut App, hwnd: HWND, mode: &'static str) {
    if app.ui.mode == mode {
        return;
    }
    app.ui.mode = mode;
    *app.state.mode.lock().unwrap() = mode.to_string();
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
            let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let mut pt = POINT { x: ((lparam.0) & 0xFFFF) as u16 as i16 as i32, y };
            let _ = ScreenToClient(hwnd, &mut pt);
            if let Some(app) = ptr.as_ref() {
                if (pt.y as f32) < 36.0 * app.scale {
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
                    app.guardian.check(&app.state, hwnd);
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

        // Estado compartido (el MISMO que la variante Tauri).
        let state = Arc::new(AppState::default());
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

        // Ventana frameless siempre visible, arriba a la derecha del primario.
        let cx = GetSystemMetrics(SM_CXSCREEN);
        let (lw, lh) = ui::mode_size("dev");
        let (pw, ph) = (logical_to_physical(lw, scale), logical_to_physical(lh, scale));
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name,
            w!("System Monitor Widget"),
            WS_POPUP,
            cx - pw - 24,
            96,
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
        app.state.visible.store(true, Ordering::SeqCst);

        // Temporizadores + hotkey + bandeja.
        SetTimer(Some(hwnd), TIMER_METRICS, METRICS_MS, None);
        RegisterHotKey(Some(hwnd), HOTKEY_ID, HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_SHIFT.0), VK_M.0 as u32)?;
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
