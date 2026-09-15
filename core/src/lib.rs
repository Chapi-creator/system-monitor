//! sysmon-core — Núcleo compartido del widget, SIN dependencia de UI.
//!
//! Lo consumen ambos binarios:
//!  - el binario Tauri (src-tauri) vía IPC hacia el WebView, y
//!  - el binario NATIVO (native) que dibuja la UI con Win32+Direct2D.

pub mod format;
pub mod kill;
pub mod sampler;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

// ---------------------------------------------------------------------------
// Geometría del widget (unidades lógicas, idénticas en ambas variantes).
// ---------------------------------------------------------------------------
pub const WIDGET_W: f64 = 340.0;
pub const WIDGET_H: f64 = 500.0;
pub const MINI_H: f64 = 245.0;

/// Snapshot de métricas (camelCase) para el renderer.
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

/// E/S de disco físico agregada (B/s). Sentinel -1 = contadores aún no
/// disponibles (el 1er renglón PDH llega ~2 s tras el arranque).
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    pub read_bytes_sec: f64,
    pub write_bytes_sec: f64,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub ok: bool,
    pub cpu: f64,
    pub memory: MemoryInfo,
    pub network: NetworkInfo,
    pub disk: DiskInfo,
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
    /// Ruta completa del ejecutable (None si el SO la oculta): alimenta el
    /// tooltip de la fila del Top-5. La resuelve sysinfo con `with_exe`.
    pub exe: Option<String>,
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

/// Estadísticas de sesión en memoria: máximos y promedios desde el arranque
/// de la app. Viven en Rust y se calculan con las muestras que spawn_stats
/// YA toma cada segundo: costo extra ≈ 0.
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub samples: u64,
    #[serde(skip)]
    pub cpu_sum: f64,
    #[serde(skip)]
    pub mem_sum: f64,
    #[serde(skip)]
    pub gpu_sum: f64,
    #[serde(skip)]
    pub net_sum: f64,
    #[serde(skip)]
    pub gpu_samples: u64,
    pub cpu_max: f64,
    pub mem_max: f64,
    pub gpu_max: f64,
    pub net_max: f64,
}

impl SessionStats {
    /// Registra una muestra. `gpu < 0` = sin dato (typeperf aún calibrando):
    /// se ignora para GPU pero NO invalida el resto de la muestra.
    pub fn record(&mut self, cpu: f64, mem: f64, gpu: f64, net_bps: f64) {
        self.samples += 1;
        self.cpu_sum += cpu;
        self.mem_sum += mem;
        self.net_sum += net_bps;
        self.cpu_max = self.cpu_max.max(cpu);
        self.mem_max = self.mem_max.max(mem);
        self.net_max = self.net_max.max(net_bps);
        if gpu >= 0.0 {
            self.gpu_sum += gpu;
            self.gpu_samples += 1;
            self.gpu_max = self.gpu_max.max(gpu);
        }
    }

    pub fn cpu_avg(&self) -> f64 {
        if self.samples == 0 { 0.0 } else { self.cpu_sum / self.samples as f64 }
    }

    pub fn mem_avg(&self) -> f64 {
        if self.samples == 0 { 0.0 } else { self.mem_sum / self.samples as f64 }
    }

    pub fn gpu_avg(&self) -> f64 {
        if self.gpu_samples == 0 { 0.0 } else { self.gpu_sum / self.gpu_samples as f64 }
    }

    pub fn net_avg(&self) -> f64 {
        if self.samples == 0 { 0.0 } else { self.net_sum / self.samples as f64 }
    }
}

pub struct GpuInfoCache {
    pub at: Instant,
    pub info: GpuInfo,
}

/// Estado global accesible por comandos (tauri State o la ventana nativa) y
/// por los hilos de muestreo.
pub struct AppState {
    pub snapshot: Mutex<Snapshot>,
    pub procs: Mutex<Vec<ProcInfo>>,
    /// Uso de GPU en % × 10 (i32 evita f64 atómico): -10 = sin dato.
    pub gpu_util: AtomicI32,
    pub temp: Mutex<TempState>,
    pub gpu_info: Mutex<GpuInfoCache>,
    pub pinned: AtomicBool,
    pub mode: Mutex<String>,
    /// Ventana visible: los hilos de muestreo caros se gatean con esto.
    pub visible: AtomicBool,
    /// E/S de disco agregada en B/s (i64 evita f64 atómico): -1 = sin dato.
    pub disk_read: AtomicI64,
    pub disk_write: AtomicI64,
    /// Máximos/promedios desde el arranque (fuente de verdad: Rust).
    pub session: Mutex<SessionStats>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(Snapshot {
                ok: true,
                temp_status: "none".into(),
                gpu: -1.0,
                disk: DiskInfo { read_bytes_sec: -1.0, write_bytes_sec: -1.0 },
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
            visible: AtomicBool::new(true),
            disk_read: AtomicI64::new(-1),
            disk_write: AtomicI64::new(-1),
            session: Mutex::new(SessionStats::default()),
        }
    }
}

// ---------------------------------------------------------------------------
// Utilidades
// ---------------------------------------------------------------------------
pub fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

pub fn round2(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// Evita que un PID malicioso derrame bytes nulos en el nombre del proceso.
pub fn sanitize(s: &str) -> String {
    s.replace(['\0', '\r', '\n'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_stats_registra_maximos_y_promedios() {
        let mut s = SessionStats::default();
        s.record(10.0, 40.0, 5.0, 1024.0);
        s.record(30.0, 60.0, -1.0, 2048.0); // gpu -1 = sin dato: se ignora para GPU
        s.record(20.0, 50.0, 25.0, 512.0);
        assert_eq!(s.samples, 3);
        assert_eq!(s.gpu_samples, 2);
        assert_eq!(s.cpu_max, 30.0);
        assert_eq!(s.gpu_max, 25.0);
        assert!((s.cpu_avg() - 20.0).abs() < 1e-9);
        assert!((s.gpu_avg() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn session_stats_vacia_no_divide_por_cero() {
        let s = SessionStats::default();
        assert_eq!(s.cpu_avg(), 0.0);
        assert_eq!(s.gpu_avg(), 0.0);
    }

    #[test]
    fn utilidades_sanitize_y_round() {
        assert_eq!(sanitize("explorer.exe"), "explorer.exe");
        assert_eq!(sanitize("bad\0name"), "bad name");
        assert_eq!(sanitize("weird\r\nname"), "weird  name");
        assert_eq!(round1(23.44), 23.4);
        assert_eq!(round1(23.45), 23.5);
        assert_eq!(round2(7.006), 7.01);
        assert_eq!(round1(-1.25), -1.3);
    }
}
