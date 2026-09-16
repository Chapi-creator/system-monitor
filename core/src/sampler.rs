//! sampler.rs — Hilos de muestreo en segundo plano (port de main.js).
//!
//!  - stats: CPU % (deltas de ticks del kernel vía sysinfo), RAM efectiva
//!    (total − available), red diferencial por interfaz activa → 1 s.
//!  - procs: top-5 por CPU → 10 s.
//!  - gpu:   typeperf PERSISTENTE (contadores PDH por motor) → cada 2 s.
//!  - temp:  WMI MSAcpi_ThermalZoneTemperature (namespace ROOT\WMI) → cada
//!    60 s, con estado
//!    honesto de 3 niveles ('ok' | 'admin' | 'none').
//!  - disk:  typeperf PERSISTENTE (PhysicalDisk Read/Write Bytes/sec) → 2 s.
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

/// El renderer (pollTick) y el timer nativo consumen el snapshot cada 2500 ms:
/// muestrear más rápido es trabajo que nadie lee. La matemática de B/s divide
/// por `elapsed`, así que sigue exacta con cualquier cadencia.
const STATS_INTERVAL: Duration = Duration::from_millis(2500);
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

/// Lanza los 5 hilos de muestreo (uno por fuente, ninguno bloquea a otro).
pub fn spawn_all(state: Arc<AppState>) {
    spawn_stats(state.clone());
    spawn_procs(state.clone());
    spawn_gpu(state.clone());
    spawn_disk(state.clone());
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
            // cada 2.5 s en ese estado es trabajo 100% desperdiciado (y era el consumo
            // base dominante en oculto). Dormimos sin muestrear; al reaparecer, la
            // ventana `elapsed` grande produce una 1ª lectura válida al instante.
            if !state.visible.load(Ordering::SeqCst) {
                thread::sleep(STATS_INTERVAL);
                // prev_time NO se toca aquí: al reaparecer, `elapsed` cubre todo
                // el período oculto y los deltas de red/CPU se dividen entre esa
                // ventana → 1ª lectura = promedio real. Si se reseteara aquí,
                // esos mismos bytes se dividirían por ~16 ms → pico falso de
                // velocidad de red gigante al restaurar el widget.
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

            // Disco: publicado por el hilo de typeperf en atómicos (sin locks).
            let d_read = state.disk_read.load(Ordering::SeqCst) as f64 / 10.0;
            let d_write = state.disk_write.load(Ordering::SeqCst) as f64 / 10.0;
            let disk = crate::DiskInfo {
                read_bytes_sec: if d_read >= 0.0 { round1(d_read) } else { -1.0 },
                write_bytes_sec: if d_write >= 0.0 { round1(d_write) } else { -1.0 },
            };

            // Estadísticas de sesión: MISMA muestra, costo extra ≈ 0.
            // Gating honesto: solo se registra lo que el usuario pudo ver.
            let net_bps = rx + tx;
            state.session.lock().unwrap().record(cpu, used / total * 100.0, gpu, net_bps);

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
                disk,
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
                // CPU + RAM siempre; `exe` solo si aún no está resuelta
                // (OnlyIfNotSet): la ruta se consulta al SO UNA vez por
                // proceso y luego queda cacheada por sysinfo — así el
                // tooltip muestra la ruta completa sin reabrir la peor
                // parte del pico de enumeración cada 10 s.
                ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_memory()
                    .with_exe(sysinfo::UpdateKind::OnlyIfNotSet),
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
                    // Ruta del exe (para el tooltip): Some solo si el SO la
                    // entregó; sistemas protegidos → None y la UI degrada.
                    let exe = p.exe().map(|path| sanitize(&path.to_string_lossy()));
                    Some(ProcInfo {
                        pid,
                        name,
                        cpu: round1(p.cpu_usage() as f64),
                        mem: round2(mem),
                        exe,
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

/// Lanza `typeperf` con los contadores dados dentro del Job Object
/// kill-on-close (si el widget muere, el kernel mata al hijo: sin huérfanos).
#[cfg(windows)]
fn spawn_typeperf_counters(
    counters: &[&str],
    interval_secs: u32,
) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new("typeperf");
    cmd.args(counters)
        .args(["-si", &interval_secs.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Se lanza dentro de un Job Object kill-on-close: si el widget muere de
    // cualquier forma, el kernel mata a typeperf con él (no queda huérfano).
    spawn_child_in_kill_on_close_job(&mut cmd)
}

#[cfg(windows)]
fn spawn_typeperf() -> std::io::Result<std::process::Child> {
    spawn_typeperf_counters(&[r"\GPU Engine(*)\Utilization Percentage"], 2)
}

#[cfg(not(windows))]
fn spawn_typeperf_counters(
    _counters: &[&str],
    _interval_secs: u32,
) -> std::io::Result<std::process::Child> {
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
    // OJO: la clase vive en ROOT\WMI, NO en el default ROOT\CIMV2. Con el
    // namespace por defecto la conexión tiene éxito pero la clase no existe →
    // lista vacía → equipos CON sensor mostraban "Sin sensor" incluso como
    // admin. ROOT\WMI es lo que usaba la variante Electron.
    let com = COMLibrary::new().ok()?;
    let conn = wmi::WMIConnection::with_namespace_path("ROOT\\WMI", com).ok()?;
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

    #[test]
    fn parse_disk_csv_line_agrupa_por_contador() {
        // Layout real: [ts, R_0, R_1, W_0, W_1] — lecturas juntas, escrituras
        // juntas (verificado en vivo: 2 contadores × 2 discos).
        let line = "\"08/09/2026 10:00:00.000\",\"100\",\"50\",\"200\",\"25\"";
        let (r, w) = parse_disk_csv_line(line).unwrap();
        assert_eq!(r, 150.0); // 100 + 50
        assert_eq!(w, 225.0); // 200 + 25
    }

    #[test]
    fn parse_disk_csv_line_un_solo_disco_y_valores_invalidos() {
        // Un disco: [ts, R, W].
        let line = "\"t\",\"1024\",\"2048\"";
        let (r, w) = parse_disk_csv_line(line).unwrap();
        assert_eq!((r, w), (1024.0, 2048.0));
        // Valores no numéricos se descartan; cantidad impar → None (layout
        // desconocido, mejor no inventar).
        assert!(parse_disk_csv_line("\"t\",\"x\",\"y\"").is_none());
        assert!(parse_disk_csv_line("\"t\",\"5\",\"malo\",\"7\"").is_none());
    }

    #[test]
    fn parse_disk_csv_line_rechaza_basura_y_cabecera() {
        assert!(parse_disk_csv_line("sin comillas").is_none());
        assert!(parse_disk_csv_line("\"(PDH-CSV 4.0)\",\"1\",\"2\"").is_none());
        assert!(parse_disk_csv_line("\"t\"").is_none());
        assert!(parse_disk_csv_line("").is_none());
    }

    #[test]
    fn session_stats_registra_maximos_y_promedios() {
        let mut s = crate::SessionStats::default();
        s.record(10.0, 40.0, 5.0, 1024.0);
        s.record(30.0, 60.0, -1.0, 2048.0); // gpu -1 = sin dato: se ignora para GPU.
        s.record(20.0, 50.0, 25.0, 512.0);
        assert_eq!(s.samples, 3);
        assert_eq!(s.gpu_samples, 2); // la muestra sin GPU no cuenta para GPU.
        assert_eq!(s.cpu_max, 30.0);
        assert_eq!(s.mem_max, 60.0);
        assert_eq!(s.gpu_max, 25.0);
        assert_eq!(s.net_max, 2048.0);
        assert!((s.cpu_avg() - 20.0).abs() < 1e-9);
        assert!((s.gpu_avg() - 15.0).abs() < 1e-9);
        assert!((s.net_avg() - (1024.0 + 2048.0 + 512.0) / 3.0).abs() < 1e-6);
    }

    #[test]
    fn session_stats_vacia_no_divide_por_cero() {
        let s = crate::SessionStats::default();
        assert_eq!(s.cpu_avg(), 0.0);
        assert_eq!(s.mem_avg(), 0.0);
        assert_eq!(s.gpu_avg(), 0.0);
        assert_eq!(s.net_avg(), 0.0);
    }

    #[test]
    fn stats_interval_iguala_cadencia_de_consumo() {
        // pollTick (Tauri) y TIMER_METRICS (nativo) leen el snapshot cada
        // 2500 ms: si alguien baja esta constante, vuelve el trabajo que
        // nadie lee. Este test lo hace fallar a propósito.
        assert_eq!(STATS_INTERVAL, Duration::from_millis(2500));
    }

    /// Integración real: los contadores de disco deben resolverse en ESTA
    /// máquina (registro Perflib) a 2 rutas distintas y bien formadas,
    /// sea cual sea el idioma del sistema. En un sistema español verifica
    /// explícitamente la traducción — regression del bug del nulo final:
    /// sin él, la lectura del registro fallaba y el fallback inglés
    /// "funcionaba" en el test pero mataba typeperf en vivo.
    #[cfg(windows)]
    #[test]
    fn disk_counters_resuelve_dos_rutas_validas() {
        let c = disk_counters::get();
        assert!(c[0].starts_with('\\') && !c[0].is_empty(), "read={}", c[0]);
        assert!(c[1].starts_with('\\') && !c[1].is_empty(), "write={}", c[1]);
        assert_ne!(c[0], c[1]);
        if disk_counters::system_lcid() == 0x0C0A {
            assert!(
                c[0].contains("Disco f\u{00ED}sico") && c[0].contains("Bytes de lectura"),
                "en es-ES esperaba el nombre localizado, obtuve: {}",
                c[0]
            );
        }
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

// ---------------------------------------------------------------------------
// Disco: E/S física agregada vía PDH (typeperf persistente, 1 hijo aparte).
// Los contadores de disco físico NO se pueden pedir en la misma consulta de
// GPU Engine, así que es un segundo typeperf con su propio gating de pausa.
// ---------------------------------------------------------------------------

/// Resuelve los nombres LOCALIZADOS de los contadores de disco.
///
/// A diferencia de "GPU Engine" (no localizado), los contadores clásicos de
/// disco se traducen por Windows: en un sistema en español pedir
/// "\PhysicalDisk(*)\Disk Read Bytes/sec" produce "Error: contadores no
/// válidos". La vía robusta e independiente del idioma es el registro de
/// Perflib: la tabla inglesa (009) da el índice numérico de cada nombre y la
/// tabla del idioma del sistema (LCID hex, p.ej. 0C0A) lo traduce.
#[cfg(windows)]
mod disk_counters {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::sync::OnceLock;

    use windows_sys::Win32::Globalization::GetSystemDefaultUILanguage;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    pub const PERFLIB_PATH: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Perflib";
    /// Nombre del valor MULTI_SZ, con nulo final (exigido por la API).
    const COUNTER_VALUE: &[u16] = &[
        'C' as u16, 'o' as u16, 'u' as u16, 'n' as u16, 't' as u16, 'e' as u16, 'r' as u16, 0,
    ];

    /// UTF-16 + nulo final: RegGetValueW exige PCWSTR terminado en nulo —
    /// sin el 0, la lectura falla (y caeríamos al fallback inglés).
    pub fn wide_nul(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// LCID del idioma del sistema (0x0409 = inglés, 0x0C0A = español...).
    pub fn system_lcid() -> u16 {
        unsafe { GetSystemDefaultUILanguage() }
    }

    /// Subclave Perflib del idioma local, con CASCADA de candidatos:
    /// el naming clásico usa hex de 3 dígitos ("009" inglés, "00A" español
    /// tradicional, "407" alemán) y algunos sistemas agrupan por LANGID
    /// primario (es-ES moderno 0x0C0A vive en "00A", no en "C0A"/"0C0A").
    /// Devuelve la primera tabla que exista.
    pub fn localized_table() -> Option<Vec<String>> {
        let lcid = system_lcid();
        let candidates = [
            format!("{PERFLIB_PATH}\\{:03X}", lcid),
            format!("{PERFLIB_PATH}\\{:03X}", lcid & 0x3FF), // LANGID primario
            format!("{PERFLIB_PATH}\\{:04X}", lcid),
        ];
        for c in candidates {
            if let Some(t) = read_multi_sz(&wide_nul(&c)) {
                return Some(t);
            }
        }
        None
    }

    /// Lee un valor MULTI_SZ del registro (UTF-16 → Vec<String>).
    ///
    /// Vía abierta+query y NO RegGetValueW: los valores `Counter` de Perflib
    /// están mal formados como MULTI_SZ (terminador doble-nulo irregular) y
    /// RegGetValueW los rechaza por estricto, mientras que RegQueryValueExW
    /// (el que usa PowerShell) los devuelve tal cual.
    pub fn read_multi_sz(subkey: &[u16]) -> Option<Vec<String>> {
        let mut hkey: HKEY = std::ptr::null_mut();
        let rc = unsafe {
            RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut hkey)
        };
        if rc != 0 {
            return None;
        }
        let result = (|| {
            let mut size: u32 = 0;
            let mut kind: u32 = 0;
            let rc = unsafe {
                RegQueryValueExW(
                    hkey,
                    COUNTER_VALUE.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    std::ptr::null_mut(),
                    &mut size,
                )
            };
            if rc != 0 || size == 0 {
                return None;
            }
            let mut buf = vec![0u16; (size / 2) as usize + 1];
            let mut got = size;
            let rc = unsafe {
                RegQueryValueExW(
                    hkey,
                    COUNTER_VALUE.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buf.as_mut_ptr() as *mut u8,
                    &mut got,
                )
            };
            if rc != 0 {
                return None;
            }
            let wide = &buf[..(got / 2) as usize];
            let os = OsString::from_wide(wide);
            Some(
                os.to_string_lossy()
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
            )
        })();
        unsafe { RegCloseKey(hkey) };
        result
    }

    /// La tabla es [índice, nombre, índice, nombre, ...] → mapa índice→nombre.
    pub fn index_map(table: &[String]) -> Option<std::collections::HashMap<u32, String>> {
        if table.len() < 2 {
            return None;
        }
        let mut map = std::collections::HashMap::new();
        let mut i = 0;
        while i + 1 < table.len() {
            if let Ok(idx) = table[i].trim().parse::<u32>() {
                map.insert(idx, table[i + 1].clone());
            }
            i += 2;
        }
        Some(map)
    }

    /// Traduce la ruta inglesa completa con la tabla del idioma local:
    /// cada componente inglesa (objeto "PhysicalDisk" y contador "Disk Read
    /// Bytes/sec") se mapea a su índice en la tabla 009 y se sustituye por el
    /// nombre con ese mismo índice en la tabla localizada.
    fn localize(path: &str, map_en: &std::collections::HashMap<u32, String>, map_loc: &std::collections::HashMap<u32, String>) -> Option<String> {
        let mut out = String::new();
        let mut replaced = 0;
        for part in path.split('\\').filter(|p| !p.is_empty()) {
            // Base del nombre sin "(*)": los índices aplican al objeto base.
            let needle = part.split('(').next().unwrap_or(part).trim();
            let idx = map_en
                .iter()
                .find(|(_, v)| v.as_str() == needle)
                .map(|(k, _)| *k)
                .and_then(|k| map_loc.get(&k).cloned());
            out.push('\\');
            match idx {
                Some(loc) => {
                    out.push_str(&part.replacen(needle, &loc, 1));
                    replaced += 1;
                }
                None => out.push_str(part),
            }
        }
        // Solo confiamos en la traducción si TODAS las componentes se mapearon.
        (replaced >= 2 && !out.is_empty()).then_some(out)
    }

    /// Contadores de disco resueltos UNA vez por proceso (OnceLock): lectura
    /// de dos tablas de registro ~3000 pares, solo en el arranque del hilo.
    pub fn get() -> &'static [String; 2] {
        static DISK: OnceLock<[String; 2]> = OnceLock::new();
        DISK.get_or_init(|| {
            const READ_EN: &str = r"\PhysicalDisk(*)\Disk Read Bytes/sec";
            const WRITE_EN: &str = r"\PhysicalDisk(*)\Disk Write Bytes/sec";
            let resolved = (|| {
                // Subclaves SIEMPRE con nulo final (wide_nul).
                let sub_009 = wide_nul(&format!("{PERFLIB_PATH}\\009"));
                let map_en = index_map(&read_multi_sz(&sub_009)?)?;
                let map_loc = index_map(&localized_table()?)?;
                Some((
                    localize(READ_EN, &map_en, &map_loc)?,
                    localize(WRITE_EN, &map_en, &map_loc)?,
                ))
            })();
            match resolved {
                Some((r, w)) => [r, w],
                // Fallback: nombres ingleses (correctos en Windows en inglés).
                _ => [READ_EN.to_string(), WRITE_EN.to_string()],
            }
        })
    }
}

fn spawn_disk(state: Arc<AppState>) {
    thread::spawn(move || {
        // Nombres localizados resueltos UNA vez (Perflib del registro): en un
        // Windows en español pedir los nombres ingleses da "contadores no
        // válidos" y el hilo moriría sin dato de disco.
        let resolved = disk_counters::get();
        let counters: Vec<&str> = resolved.iter().map(|s| s.as_str()).collect();
        let mut restarts = 0u32;
        loop {
            let mut child = match spawn_typeperf_counters(&counters, 2) {
                Ok(c) => c,
                Err(_) => {
                    // typeperf inexistente: sin vía de contadores de disco.
                    state.disk_read.store(-1, Ordering::SeqCst);
                    state.disk_write.store(-1, Ordering::SeqCst);
                    return;
                }
            };
            let stdout = child.stdout.take().expect("typeperf stdout");
            // Lectura tolerante: los NOMBRES localizados de los contadores en la
            // cabecera PDH-CSV vienen en la ANSI codepage del sistema (p.ej.
            // "Disco físico" en CP1252, byte 0xED = UTF-8 INVÁLIDO).
            // reader.lines() + map_while(Result::ok) abortaba ahí silenciosamente
            // en la primera línea → hijo "muerto" → 3 reintentos → rendición.
            let mut raw = BufReader::new(stdout);

            // GATING idéntico al de GPU: oculto en bandeja, typeperf se SUSPENDE
            // (una sola vez por episodio — NtSuspendProcess apila contadores) y
            // se reanuda al volver. El flujo PDH se conserva sin recalibrar.
            let pause = winapi_pause::PauseGuard::new(&mut child);
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

            // Búfer propio + read_until: tolera bytes no-UTF8 (lossy) sin
            // cortar el flujo nunca.
            let mut buf: Vec<u8> = Vec::with_capacity(4096);
            loop {
                buf.clear();
                let n = raw.read_until(b'\n', &mut buf).unwrap_or(0);
                if n == 0 {
                    break; // EOF: el hijo cerró stdout.
                }
                let line = String::from_utf8_lossy(&buf);
                if line.starts_with("\"(PDH-CSV") {
                    continue; // Cabecera: los contadores de disco van fijos por índice.
                }
                if let Some((read, write)) = parse_disk_csv_line(&line) {
                    // ×10 en i64: mismo truco que gpu_util para evitar f64 atómico.
                    state.disk_read.store((read * 10.0).round() as i64, Ordering::SeqCst);
                    state.disk_write.store((write * 10.0).round() as i64, Ordering::SeqCst);
                }
            }
            // El hijo salió (stdout cerrado): se recolecta y se reintenta con tope.
            let _ = child.kill();
            let _ = child.wait();
            if restarts >= GPU_MAX_RESTARTS {
                state.disk_read.store(-1, Ordering::SeqCst);
                state.disk_write.store(-1, Ordering::SeqCst);
                return;
            }
            restarts += 1;
            thread::sleep(GPU_RESTART_DELAY);
        }
    });
}

/// Parsea una línea CSV de typeperf de disco → (read B/s, write B/s).
///
/// LAYOUT REAL (verificado en vivo): typeperf emite los valores AGRUPADOS POR
/// CONTADOR, no por disco — con 2 contadores pedidos y N discos físicos:
///   [ts, R_disco0, R_disco1, ..., W_disco0, W_disco1, ...]
/// Con N impar (un solo disco) todo sigue encajando: la mitad de las columnas
/// numéricas es lectura y la otra mitad escritura.
/// AGREGACIÓN: se SUMAN todas las instancias de cada contador (E/S total del
/// sistema, igual que el Administrador de Tareas en su pestaña Rendimiento).
fn parse_disk_csv_line(line: &str) -> Option<(f64, f64)> {
    if !line.starts_with('"') {
        return None;
    }
    let cols = split_csv(line);
    if cols.len() < 3 || cols[0].is_empty() || cols[0].starts_with("(PDH-CSV") {
        return None;
    }
    let mut values: Vec<f64> = Vec::with_capacity(cols.len() - 1);
    for c in &cols[1..] {
        let t = c.trim();
        match t.parse::<f64>() {
            // Negativos (no deberían existir) se clampan a 0.
            Ok(v) if v.is_finite() => values.push(v.max(0.0)),
            // Columna no numérica → layout desconocido: mejor no inventar.
            _ => return None,
        }
    }
    if values.is_empty() || values.len() % 2 != 0 {
        return None;
    }
    let half = values.len() / 2;
    let read: f64 = values[..half].iter().sum();
    let write: f64 = values[half..].iter().sum();
    Some((read, write))
}