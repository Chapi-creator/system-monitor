//! sampler.rs — Hilos de muestreo en segundo plano (port de main.js).
//!
//!  - stats: CPU % (deltas de ticks del kernel vía sysinfo), RAM efectiva
//!    (total − available), red diferencial por interfaz activa → 1 s.
//!  - procs: top-5 por CPU → 10 s.
//!  - gpu:   typeperf PERSISTENTE (contadores PDH por motor) → cada 2 s.
//!  - temp:  WMI MSAcpi_ThermalZoneTemperature → cada 60 s, con estado
//!    honesto de 3 niveles ('ok' | 'admin' | 'none').
//!  - gpu_info: metadatos de GPU vía WMI Win32_VideoController (TTL 10 min).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sysinfo::{Networks, ProcessRefreshKind, ProcessesToUpdate, System};
use wmi::COMLibrary;

use crate::{round1, round2, sanitize, AppState, GpuInfo, ProcInfo, Snapshot};

// ---------------------------------------------------------------------------
// Pausa/reanudación de typeperf vía Win32 (NtSuspendProcess/NtResumeProcess).
// Pausar el proceso (en vez de matarlo) conserva los contadores PDH abiertos:
// al reanudar, el flujo continúa sin período de calibración.
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod winapi_pause {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtSuspendProcess(handle: isize) -> i32;
        fn NtResumeProcess(handle: isize) -> i32;
    }

    /// Empaqueta las llamadas ntdll para un `Child`. El kernel cuenta las
    /// suspensiones (cada NtSuspendProcess incrementa el contador por hilo y un
    /// solo NtResumeProcess lo decrementa en 1): el llamador DEBE invocar
    /// suspend()/resume() una sola vez por episodio de ocultación, no en bucle.
    pub struct PauseGuard<'a> {
        child: &'a mut Child,
    }

    impl<'a> PauseGuard<'a> {
        pub fn new(child: &'a mut Child) -> Self {
            Self { child }
        }

        pub fn suspend(&self) {
            unsafe {
                NtSuspendProcess(self.child.as_raw_handle() as isize);
            }
        }

        pub fn resume(&self) {
            unsafe {
                NtResumeProcess(self.child.as_raw_handle() as isize);
            }
        }
    }
}

#[cfg(not(windows))]
mod winapi_pause {
    /// Stub no-Windows: sin pausa real (typeperf no existe fuera de Windows).
    pub struct PauseGuard<'a> {
        _child: &'a mut std::process::Child,
    }

    impl<'a> PauseGuard<'a> {
        pub fn new(child: &'a mut std::process::Child) -> Self {
            Self { _child: child }
        }
        pub fn suspend(&self) {}
        pub fn resume(&self) {}
    }
}

// ---------------------------------------------------------------------------
// Job Object «kill-on-close»: si el widget muere de CUALQUIER forma (Quit de la
// bandeja, crash, taskkill del usuario, panic = abort), el kernel mata a
// typeperf con él. Windows NO mata hijos al morir el padre: sin esto, cada
// ejecución deja un typeperf.exe huérfano muestreando PDH para siempre.
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod job_object {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Job del proceso vivo durante toda la app (se cierra solo al morir).
    pub fn spawn_child_in_kill_on_close_job(cmd: &mut Command) -> std::io::Result<Child> {
        unsafe {
            let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok_limit = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok_limit == 0 {
                let err = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(err);
            }
            let mut child = cmd.spawn()?;
            let ok_assign = AssignProcessToJobObject(job, child.as_raw_handle() as _);
            if ok_assign == 0 {
                let err = std::io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                CloseHandle(job);
                return Err(err);
            }
            // El job NO se cierra manualmente: el kernel lo libera (matando a
            // typeperf) cuando el proceso del widget termina, sin importar cómo.
            Ok(child)
        }
    }
}

#[cfg(windows)]
use job_object::spawn_child_in_kill_on_close_job;

const STATS_INTERVAL: Duration = Duration::from_secs(1);
const PROC_INTERVAL: Duration = Duration::from_secs(10);
/// Sondeo de estado mientras el gating tiene la enumeración de procesos en pausa
/// (ventana oculta o modo sin lista): reanuda el muestreo en ≤2 s al volver.
const PROC_IDLE_POLL: Duration = Duration::from_secs(2);
const TEMP_INTERVAL: Duration = Duration::from_secs(60);
const GPU_MAX_RESTARTS: u32 = 3;
const GPU_RESTART_DELAY: Duration = Duration::from_secs(10);

/// Adaptadores virtuales a omitir (Docker, Hyper-V/WSL, VMware, VPN-TAP...).
fn is_virtual_iface(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "docker", "vethernet", "vmware", "virtualbox", "loopback", "wsl", "hyper-v", "tap", "tun",
        "bluetooth", "hibernate", "teredo",
    ]
    .iter()
    .any(|k| n.contains(k))
}

/// Lanza los 4 hilos de muestreo (uno por fuente, ninguno bloquea a otro).
pub fn spawn_all(state: Arc<AppState>) {
    spawn_stats(state.clone());
    spawn_procs(state.clone());
    spawn_gpu(state.clone());
    spawn_temp(state.clone());
}

// ---------------------------------------------------------------------------
// CPU / RAM / Red
// ---------------------------------------------------------------------------
fn spawn_stats(state: Arc<AppState>) {
    thread::spawn(move || {
        let mut sys = System::new();
        // sysinfo 0.33: las redes viven en su propia estructura (Networks).
        let mut networks = Networks::new_with_refreshed_list();
        // Warm-up de CPU: la 1ª lectura de sysinfo es la línea base (0%).
        sys.refresh_cpu_usage();
        thread::sleep(Duration::from_millis(350));

        let mut prev_time = Instant::now();

        loop {
            // GATING por visibilidad: con el widget oculto en bandeja el renderer
            // está pausado (document.hidden) y NADIE consume el snapshot. Muestrear
            // a 1 Hz en ese estado es trabajo 100% desperdiciado (y era el consumo
            // base dominante en oculto). Dormimos sin muestrear; al reaparecer, la
            // ventana `elapsed` grande produce una 1ª lectura válida al instante.
            if !state.visible.load(Ordering::SeqCst) {
                thread::sleep(STATS_INTERVAL);
                prev_time = Instant::now();
                continue;
            }
            let now = Instant::now();
            let elapsed = now.duration_since(prev_time).as_secs_f64().max(0.001);
            prev_time = now;

            sys.refresh_cpu_usage();
            sys.refresh_memory();
            // true = descarta interfaces que ya no existen (VPN/WSL desmontados).
            networks.refresh(true);

            let cpu = sys.global_cpu_usage() as f64;
            let total = sys.total_memory() as f64;
            let available = sys.available_memory() as f64;
            let used = (total - available).max(0.0);

            // NetworkData::received()/transmitted() = bytes DESDE el último
            // refresh: dividir por el intervalo da B/s sin rastrear deltas.
            let (iface, rx, tx) = pick_network(&networks, elapsed);
            let (temp_c, temp_status) = {
                let t = state.temp.lock().unwrap();
                (t.celsius, t.status.clone())
            };
            let gpu = state.gpu_util.load(Ordering::SeqCst) as f64 / 10.0;

            let snapshot = Snapshot {
                ok: true,
                cpu: round1(cpu),
                memory: crate::MemoryInfo {
                    percent: round1(if total > 0.0 { used / total * 100.0 } else { 0.0 }),
                    used_gb: round2(used / 1024.0_f64.powi(3)),
                    total_gb: round2(total / 1024.0_f64.powi(3)),
                },
                network: crate::NetworkInfo {
                    iface,
                    rx_bytes_sec: round1(rx),
                    tx_bytes_sec: round1(tx),
                },
                cpu_temp: temp_c,
                temp_status,
                gpu,
                error: None,
            };

            *state.snapshot.lock().unwrap() = snapshot;
            thread::sleep(STATS_INTERVAL);
        }
    });
}

/// Selecciona la interfaz ACTIVA por tráfico real (B/s), excluyendo
/// adaptadores virtuales; fallback a la más ocupada del listado completo.
fn pick_network(networks: &Networks, elapsed: f64) -> (String, f64, f64) {
    let mut best: Option<(String, f64, f64)> = None;
    let mut fallback: Option<(String, f64, f64)> = None;
    for (name, data) in networks.list() {
        let rx = data.received() as f64 / elapsed;
        let tx = data.transmitted() as f64 / elapsed;
        let total = rx + tx;
        let candidate = (name.to_string(), rx, tx);
        if is_virtual_iface(name) {
            continue;
        }
        if fallback.as_ref().map(|(_, brx, btx)| total > brx + btx).unwrap_or(true) {
            fallback = Some(candidate.clone());
        }
        if total > 0.0 {
            if best.as_ref().map(|(_, brx, btx)| total > brx + btx).unwrap_or(true) {
                best = Some(candidate);
            }
        }
    }
    best.or(fallback).unwrap_or_else(|| ("n/a".into(), 0.0, 0.0))
}

// ---------------------------------------------------------------------------
// Top-5 procesos por CPU
// ---------------------------------------------------------------------------
fn spawn_procs(state: Arc<AppState>) {
    thread::spawn(move || {
        let mut sys = System::new();
        loop {
            // GATING: la enumeración de procesos es con diferencia la operación
            // más cara del backend (cientos de procesos × syscalls por PID) y es
            // la causa principal de los picos periódicos de CPU. Solo se hace si
            // la ventana está visible Y el modo muestra la lista (dev | procs).
            // Oculto en bandeja o en mini/charts → cero trabajo.
            let mode_allows = matches!(state.mode.lock().unwrap().as_str(), "dev" | "procs");
            if !(state.visible.load(Ordering::SeqCst) && mode_allows) {
                thread::sleep(PROC_IDLE_POLL);
                continue;
            }
            sys.refresh_cpu_usage();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                // Solo CPU + RAM: se omiten `exe` (resolución de ruta de imagen,
                // carísima) y `disk_usage` (contadores E/S) de cada proceso.
                // Esos dos campos son el grueso del pico y no se muestran.
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );

            let total_mem = sys.total_memory() as f64;
            let mut top: Vec<ProcInfo> = sys
                .processes()
                .iter()
                .filter_map(|(pid, p)| {
                    let pid = pid.as_u32();
                    let name = sanitize(&p.name().to_string_lossy());
                    if pid == 0 {
                        return None;
                    }
                    // Omite ruido de sistema (System Idle Process, etc.).
                    let lower = name.to_lowercase();
                    if lower == "system idle process" || lower == "system interrupts" {
                        return None;
                    }
                    let mem = if total_mem > 0.0 {
                        p.memory() as f64 / total_mem * 100.0
                    } else {
                        0.0
                    };
                    Some(ProcInfo {
                        pid,
                        name,
                        cpu: round1(p.cpu_usage() as f64),
                        mem: round2(mem),
                    })
                })
                .collect();

            top.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
            top.truncate(5);
            *state.procs.lock().unwrap() = top;

            thread::sleep(PROC_INTERVAL);
        }
    });
}

// ---------------------------------------------------------------------------
// GPU: uso real vía contadores PDH (typeperf persistente, igual que main.js).
// ---------------------------------------------------------------------------
fn spawn_gpu(state: Arc<AppState>) {
    thread::spawn(move || {
        let mut restarts = 0u32;
        loop {
            let mut child = match spawn_typeperf() {
                Ok(c) => c,
                Err(_) => {
                    // typeperf inexistente o no ejecutable: sin vía de contadores.
                    state.gpu_util.store(-10, Ordering::SeqCst);
                    return;
                }
            };
            // Blindaje extra: aunque algo escape del bucle sin wait(), el job
            // garantiza que no queda typeperf vivo tras la muerte del widget.
            let stdout = child.stdout.take().expect("typeperf stdout");
            let reader = BufReader::new(stdout);

            // GATING: mientras el widget está oculto, typeperf consume CPU y red
            // de contadores para un dato que nadie lee. Se pausa con SUSPEND y se
            // reanuda al volver (reconectar el flujo perdería la 1ª muestra PDH
            // igual que un reinicio, así que pausar es más barato y simple).
            let pause = winapi_pause::PauseGuard::new(&mut child);
            // Suspend UNA sola vez por episodio: NtSuspendProcess apila contadores
            // y un único resume() al reaparecer no recuperaría un bucle que
            // suspendió cada 500 ms → typeperf quedaba congelado para siempre y
            // la GPU mostraba el último % tras la primera ocultación.
            let mut suspended = false;
            while !state.visible.load(Ordering::SeqCst) {
                if !suspended {
                    pause.suspend();
                    suspended = true;
                }
                thread::sleep(Duration::from_millis(500));
            }
            if suspended {
                pause.resume();
            }

            let mut column_types: Vec<String> = Vec::new();
            for line in reader.lines().map_while(Result::ok) {
                if line.starts_with("\"(PDH-CSV") {
                    column_types = parse_header(&line);
                    continue;
                }
                if let Some(v) = parse_gpu_csv_line(&line, &column_types) {
                    state.gpu_util.store((v * 10.0).round() as i32, Ordering::SeqCst);
                }
            }
            // El hijo salió (stdout cerrado): se recolecta y se reintenta con tope.
            let _ = child.kill();
            let _ = child.wait();
            if restarts >= GPU_MAX_RESTARTS {
                state.gpu_util.store(-10, Ordering::SeqCst);
                return;
            }
            restarts += 1;
            thread::sleep(GPU_RESTART_DELAY);
        }
    });
}

#[cfg(windows)]
fn spawn_typeperf() -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new("typeperf");
    cmd.args([r"\GPU Engine(*)\Utilization Percentage", "-si", "2"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Se lanza dentro de un Job Object kill-on-close: si el widget muere de
    // cualquier forma, el kernel mata a typeperf con él (no queda huérfano).
    spawn_child_in_kill_on_close_job(&mut cmd)
}

#[cfg(not(windows))]
fn spawn_typeperf() -> std::io::Result<std::process::Child> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "typeperf is Windows-only"))
}

/// Cabecera PDH-CSV: extrae el engtype de cada columna.
fn parse_header(line: &str) -> Vec<String> {
    split_csv(line)
        .into_iter()
        .skip(1)
        .map(|h| {
            let lower = h.to_lowercase();
            if let Some(pos) = lower.find("engtype_") {
                let rest = &lower[pos + 8..];
                let end = rest.find(')').unwrap_or(rest.len());
                rest[..end].trim().to_string()
            } else {
                "unknown".into()
            }
        })
        .collect()
}

/// Parsea una línea CSV de typeperf → uso agregado de GPU en %.
/// AGREGACIÓN ESTILO ADMINISTRADOR DE TAREAS: se suman los motores del mismo
/// engtype (varios motores 3D = uso real del pipeline) y se toma el MÁXIMO
/// entre tipos (el % del chip es el del tipo más ocupado). Clamp 0-100.
fn parse_gpu_csv_line(line: &str, column_types: &[String]) -> Option<f64> {
    if !line.starts_with('"') {
        return None;
    }
    let cols = split_csv(line);
    if cols.len() < 2 || cols[0].is_empty() || cols[0].starts_with("(PDH-CSV") {
        return None;
    }
    if column_types.is_empty() {
        return None; // Aún sin cabecera: no hay cómo clasificar.
    }
    let mut by_type: HashMap<String, f64> = HashMap::new();
    for (i, col) in cols.iter().enumerate().skip(1) {
        let v: f64 = col.parse().unwrap_or(f64::NAN);
        if !v.is_finite() || v <= 0.0 {
            continue;
        }
        let t = column_types.get(i - 1).cloned().unwrap_or_else(|| "unknown".into());
        *by_type.entry(t).or_insert(0.0) += v;
    }
    let max = by_type.values().copied().fold(0.0, f64::max);
    Some(max.clamp(0.0, 100.0))
}

/// Split CSV respetando comillas (mismo algoritmo que metrics.js).
fn split_csv(line: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                cols.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    cols.push(cur);
    cols
}

// ---------------------------------------------------------------------------
// Temperatura CPU (WMI) con estado honesto de 3 niveles.
// ---------------------------------------------------------------------------
#[derive(Deserialize)]
#[serde(rename = "MSAcpi_ThermalZoneTemperature")]
struct ThermalZone {
    #[serde(rename = "CurrentTemperature")]
    current_temperature: u32, // Kelvin × 10.
}

fn spawn_temp(state: Arc<AppState>) {
    thread::spawn(move || loop {
        // GATING: la consulta WMI solo importa si alguien va a ver el dato.
        if !state.visible.load(Ordering::SeqCst) {
            thread::sleep(TEMP_INTERVAL);
            continue;
        }
        // is_elevated devuelve bool directo (chequeo por token, no `net session`).
        let elevated = is_elevated::is_elevated();
        let celsius = read_acpi_temp();
        let status = if celsius.is_some() {
            "ok"
        } else if !elevated {
            // Sin datos Y sin elevación: ambiguo (¿permisos o sin sensor?).
            "admin"
        } else {
            // Sin datos AUN elevado: el hardware no publica la zona térmica.
            "none"
        };
        *state.temp.lock().unwrap() = crate::TempState {
            celsius: celsius.unwrap_or(-1.0),
            status: status.into(),
        };
        thread::sleep(TEMP_INTERVAL);
    });
}

/// Lectura ACPI directa: decodifica Kelvin×10 → °C. None si la clase no
/// expone zonas o el valor está fuera del rango físico plausible (0-120 °C).
fn read_acpi_temp() -> Option<f64> {
    // wmi 0.17 exige inicializar COM explícitamente (COMLibrary) por hilo.
    let com = COMLibrary::new().ok()?;
    let conn = wmi::WMIConnection::new(com).ok()?;
    let zones: Vec<ThermalZone> = conn.query().ok()?;
    let kelvin_x10 = zones.first()?.current_temperature;
    let celsius = kelvin_x10 as f64 / 10.0 - 273.15;
    if (0.0..=120.0).contains(&celsius) {
        Some((celsius * 10.0).round() / 10.0)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Metadatos de GPU (WMI Win32_VideoController, TTL 10 min en el comando).
// ---------------------------------------------------------------------------
#[derive(Deserialize)]
#[serde(rename = "Win32_VideoController")]
struct VideoController {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "AdapterRAM")]
    adapter_ram: Option<u64>, // Bytes.
    #[serde(rename = "DriverVersion")]
    driver_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gpu_csv_line_agrega_por_engtype_y_toma_maximo() {
        // 3D en dos columnas (suma) y VideoDecode en otra (máximo gana).
        let types = vec!["3d".into(), "3d".into(), "videodecode".into()];
        let line = "\"08/09/2026 10:00:00.000\",\"10\",\"20\",\"30\"";
        let v = parse_gpu_csv_line(line, &types).unwrap();
        assert_eq!(v, 30.0); // max(10+20, 30)
    }

    #[test]
    fn parse_gpu_csv_line_clampa_a_100() {
        let types = vec!["3d".into()];
        let line = "\"t\",\"150\"";
        assert_eq!(parse_gpu_csv_line(line, &types).unwrap(), 100.0);
    }

    #[test]
    fn parse_gpu_csv_line_ignora_cabecera_y_valores_invalidos() {
        assert!(parse_gpu_csv_line("\"(PDH-CSV 4.0)\",\"x\"", &["3d".into()]).is_none());
        assert!(parse_gpu_csv_line("sin comillas", &["3d".into()]).is_none());
        // Valores <= 0 se saltan → 0.
        let line = "\"t\",\"0\",\"-5\"";
        assert_eq!(parse_gpu_csv_line(line, &["3d".into(), "3d".into()]).unwrap(), 0.0);
    }

    #[test]
    fn parse_header_extrae_engtype() {
        let line = "\"(PDH-CSV 4.0)\",\"\\\\HOST\\GPU Engine(pid_1_luid_0_eng_0_engtype_3D)\\Utilization Percentage\"";
        let types = parse_header(line);
        assert_eq!(types, vec!["3d"]);
    }

    #[test]
    fn is_virtual_iface_filtra_adaptadores_fantasma() {
        assert!(is_virtual_iface("vEthernet (WSL)"));
        assert!(is_virtual_iface("Loopback"));
        assert!(!is_virtual_iface("Wi-Fi"));
        assert!(!is_virtual_iface("Ethernet"));
    }
}

pub fn read_gpu_info() -> GpuInfo {
    let com = COMLibrary::new().ok();
    if let Some(com) = com {
        if let Ok(conn) = wmi::WMIConnection::new(com) {
            if let Ok(list) = conn.query::<VideoController>() {
                if let Some(c) = list.into_iter().find(|c| {
                    c.name.as_deref().map(|n| !n.is_empty()).unwrap_or(false)
                }) {
                    return GpuInfo {
                        ok: true,
                        model: c.name.unwrap_or_default(),
                        vendor: String::new(),
                        vram_mb: c.adapter_ram.map(|b| b / (1024 * 1024)),
                        driver: c.driver_version.unwrap_or_default(),
                    };
                }
            }
        }
    }
    GpuInfo {
        ok: false,
        model: "GPU".into(),
        vendor: String::new(),
        vram_mb: None,
        driver: String::new(),
    }
}