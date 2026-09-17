//! Persistencia de preferencias del widget (JSON en %APPDATA%).
//!
//! Guarda lo que el usuario configuró: modo de visualización, estado fijado,
//! umbrales del guardián y posición de la ventana. Costo: un JSON de ~200
//! bytes escrito SOLO cuando algo cambia (nada en el camino caliente).
//!
//! `load` es tolerante a dispares: archivo inexistente, corrupto o con campos
//! faltantes → defaults en lo que falte (nunca pánico ni pérdida del resto).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Umbrales del guardián proactivo (port de THRESHOLDS del renderer).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Thresholds {
    pub cpu: f64,
    pub ram: f64,
    pub gpu: f64,
    /// °C del paquete CPU.
    pub temp: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { cpu: 85.0, ram: 90.0, gpu: 90.0, temp: 80.0 }
    }
}

impl Thresholds {
    /// Clamps defensivos: un valor absurdo no debe silenciar o disparar
    /// el guardián sin sentido. CPU/RAM/GPU 1-100 %; temp 30-120 °C.
    pub fn clamped(mut self) -> Self {
        self.cpu = self.cpu.clamp(1.0, 100.0);
        self.ram = self.ram.clamp(1.0, 100.0);
        self.gpu = self.gpu.clamp(1.0, 100.0);
        self.temp = self.temp.clamp(30.0, 120.0);
        self
    }

    /// Ajusta un umbral por nombre ("cpu" | "ram" | "gpu" | "temp") con clamp.
    /// Devuelve false si el nombre es desconocido.
    pub fn set(&mut self, kind: &str, value: f64) -> bool {
        match kind {
            "cpu" => self.cpu = value,
            "ram" => self.ram = value,
            "gpu" => self.gpu = value,
            "temp" => self.temp = value,
            _ => return false,
        }
        *self = self.clamped();
        true
    }
}

/// Preferencias persistidas del widget.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Último modo de visualización ("dev" | "mini" | "charts" | "procs").
    pub mode: String,
    /// Siempre visible sobre otras apps.
    pub pinned: bool,
    /// Posición guardada de la ventana (píxeles físicos de pantalla).
    /// None = usar la posición por defecto de la esquina superior derecha.
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// Umbrales del guardián.
    pub thresholds: Thresholds,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "dev".into(),
            pinned: true,
            x: None,
            y: None,
            thresholds: Thresholds::default(),
        }
    }
}

impl Settings {
    /// Modo válido; cualquier otra cosa → "dev".
    pub fn sanitized(mut self) -> Self {
        if !matches!(self.mode.as_str(), "dev" | "mini" | "charts" | "procs") {
            self.mode = "dev".into();
        }
        self.thresholds = self.thresholds.clamped();
        self
    }

    /// Ruta del archivo de preferencias: %APPDATA%/SystemMonitorWidget/settings.json.
    /// Sin APPDATA (raro en Windows) → directorio actual (degradación honesta).
    ///
    /// SYSMON_SETTINGS_DIR: override para tests — los tests de Tauri EjECUTAN
    /// los comandos de verdad, y sin este aislamiento escribirían el settings
    /// REAL del usuario (contaminación detectada en pruebas en vivo).
    pub fn path() -> PathBuf {
        if let Some(dir) = std::env::var_os("SYSMON_SETTINGS_DIR") {
            return dir.into();
        }
        let mut p = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        p.push("SystemMonitorWidget");
        p.push("settings.json");
        p
    }

    /// Carga tolerante: cualquier problema (inexistente, JSON inválido,
    /// campos extra/faltantes) → defaults parciales. Nunca pánico.
    /// Tolera BOM inicial (editores como Notepad lo agregan al guardar
    /// UTF-8: sin esto, un settings editado a mano reseteaba todo a
    /// defaults en silencio porque serde_json rechaza el BOM).
    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return Self::default();
        };
        let text = text.trim_start_matches('﻿');
        match serde_json::from_str::<Settings>(text) {
            Ok(s) => s.sanitized(),
            // Compatibilidad parcial: si el archivo entero no matchea, intentar
            // rescatar solo los umbrales (formato de versiones previas).
            Err(_) => serde_json::from_str::<Thresholds>(text)
                .map(|t| Self { thresholds: t.clamped(), ..Default::default() })
                .unwrap_or_default(),
        }
    }

    /// Guardado atómico: escribe a .tmp y renombra (un corte de luz no deja
    /// el JSON a medias). Silencia errores: preferencias no guardadas no
    /// deben tumbar la app (el próximo cambio reintenta).
    ///
    /// rename sobre destino existente falla en Windows (`AccessDenied`),
    /// así que primero se elimina el archivo previo. El rename es la única
    /// operación no atómica de la secuencia: si el proceso muere EXACTAMENTE
    /// entre remove y rename, se pierde el settings (aceptable: el fallback
    /// de load es defaults, nunca un archivo corrupto).
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_igual_que_el_renderer_original() {
        let t = Thresholds::default();
        assert_eq!(t, Thresholds { cpu: 85.0, ram: 90.0, gpu: 90.0, temp: 80.0 });
    }

    #[test]
    fn set_con_clamp_y_nombres_validos() {
        let mut t = Thresholds::default();
        assert!(t.set("cpu", 70.0));
        assert_eq!(t.cpu, 70.0);
        // Fuera de rango → clamp.
        assert!(t.set("temp", 500.0));
        assert_eq!(t.temp, 120.0);
        assert!(t.set("gpu", -5.0));
        assert_eq!(t.gpu, 1.0);
        // Nombre desconocido → false, nada cambia.
        assert!(!t.set("red", 50.0));
    }

    #[test]
    fn roundtrip_guarda_y_carga() {
        let dir = std::env::temp_dir().join(format!("sysmon-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let s = Settings {
            mode: "charts".into(),
            pinned: false,
            x: Some(1600),
            y: Some(120),
            thresholds: Thresholds { cpu: 70.0, ..Default::default() },
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        std::fs::write(&path, &json).unwrap();

        let loaded: Settings =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded, s);
        assert_eq!(loaded.thresholds.cpu, 70.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_tolera_bom_inicial() {
        // Notepad guarda UTF-8 con BOM y serde_json lo rechaza: load() lo
        // recorta antes de parsear. Sin el recorte, un settings editado a
        // mano reseteaba TODO a defaults en silencio.
        let bom = char::from_u32(0xFEFF).unwrap();
        let with_bom = format!("{bom}{{ \"mode\": \"charts\", \"thresholds\": {{ \"cpu\": 70.0 }} }}");
        assert!(serde_json::from_str::<Settings>(&with_bom).is_err());
        let s: Settings =
            serde_json::from_str(with_bom.trim_start_matches(bom)).unwrap();
        assert_eq!(s.mode, "charts");
        assert_eq!(s.thresholds.cpu, 70.0);
    }

    #[test]
    fn json_corrupto_y_campos_faltantes_caidan_a_defaults() {
        let s: Settings = serde_json::from_str("{ esto no es json").unwrap_or_default();
        assert_eq!(s, Settings::default());

        // Campos faltantes → default del campo (serde default en la struct).
        let parcial: Settings = serde_json::from_str(r#"{ "mode": "mini" }"#).unwrap();
        assert_eq!(parcial.mode, "mini");
        assert_eq!(parcial.thresholds, Thresholds::default());
        assert!(parcial.x.is_none());
    }

    #[test]
    fn sanitize_modos_invalidos() {
        let s = Settings { mode: "fullscreen".into(), ..Default::default() }.sanitized();
        assert_eq!(s.mode, "dev");
        let s = Settings { mode: "procs".into(), ..Default::default() }.sanitized();
        assert_eq!(s.mode, "procs");
    }
}
