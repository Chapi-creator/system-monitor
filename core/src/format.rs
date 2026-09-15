//! Formateadores: port EXACTO de src/lib/metrics.js — mismo output letra a
//! letra para que la UI nativa muestre exactamente lo que mostraba el WebView.

/// Convierte a número finito; cualquier cosa no finita → 0 (toNumber).
fn to_number(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

pub fn round1(n: f64) -> f64 {
    (to_number(n) * 10.0).round() / 10.0
}

pub fn round2(n: f64) -> f64 {
    (to_number(n) * 100.0).round() / 100.0
}

/// Velocidad de red legible: ≥1 MB/s → "x.xx MB/s"; si no → "x.x KB/s".
/// No-finito → "--" (igual que metrics.js).
pub fn speed(bytes_sec: f64) -> String {
    let bytes = bytes_sec;
    if !bytes.is_finite() {
        return "--".into();
    }
    if bytes >= 1024.0 * 1024.0 {
        format!("{:.2} MB/s", bytes / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB/s", bytes / 1024.0)
    }
}

/// Velocidad de disco: sentinel -1 (o cualquier negativo) → "n/a".
pub fn disk_speed(bytes_sec: f64) -> String {
    if !bytes_sec.is_finite() || bytes_sec < 0.0 {
        return "n/a".into();
    }
    speed(bytes_sec)
}

/// Temperatura legible: sentinel -1 → "n/a"; si no, entero + "°C".
pub fn temp(celsius: f64) -> String {
    if celsius.is_finite() && celsius > 0.0 {
        format!("{:.0}°C", celsius)
    } else {
        "n/a".into()
    }
}

/// GPU legible: -1 (contadores aún no disponibles) → "n/a".
pub fn gpu(pct: f64) -> String {
    if pct.is_finite() && pct > -1.0 {
        format!("{:.0}%", pct)
    } else {
        "n/a".into()
    }
}

/// Porcentaje con 1 decimal.
pub fn pct(v: f64) -> String {
    format!("{:.1}%", round1(v))
}

/// GB con 2 decimales ("x.xx GB").
pub fn gb(v: f64) -> String {
    format!("{:.2} GB", v)
}

/// Acorta la ruta de un ejecutable para el tooltip del Top-5: conserva los
/// 2 últimos segmentos con su separador original (…\Local\app.exe).
/// Rutas cortas (≤ 2 separadores) y vacías se devuelven tal cual o con el
/// texto de reserva. Port exacto de formatExePath en metrics.js.
pub fn exe_path(exe: Option<&str>) -> String {
    let Some(s) = exe.map(str::trim).filter(|s| !s.is_empty()) else {
        return "ruta no disponible".into();
    };
    let seps = s.matches(['\\', '/']).count();
    if seps <= 2 {
        return s.to_string();
    }
    // lastIndexOf('/') sobre la normalizada apunta al MISMO índice en `s`
    // (reemplazo 1:1 de separadores) → se conserva "\carpeta\exe" íntegro.
    let norm: Vec<char> = s.chars().map(|c| if c == '\\' { '/' } else { c }).collect();
    let slash = norm.iter().rposition(|&c| c == '/').unwrap_or(0);
    let prev = norm[..slash].iter().rposition(|&c| c == '/');
    match prev {
        Some(p) => {
            let byte_idx = s.char_indices().nth(p).map(|(i, _)| i).unwrap_or(0);
            format!("…{}", &s[byte_idx..])
        }
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_coincide_con_metrics_js() {
        assert_eq!(speed(0.0), "0.0 KB/s");
        assert_eq!(speed(1536.0), "1.5 KB/s");
        assert_eq!(speed(2048.0 * 1024.0), "2.00 MB/s");
        assert_eq!(speed(f64::NAN), "--");
    }

    #[test]
    fn disk_speed_sentinel_es_na() {
        assert_eq!(disk_speed(-1.0), "n/a");
        assert_eq!(disk_speed(1024.0), "1.0 KB/s");
    }

    #[test]
    fn temp_y_gpu_sentinel() {
        assert_eq!(temp(-1.0), "n/a");
        assert_eq!(temp(72.4), "72°C");
        assert_eq!(gpu(-1.0), "n/a");
        assert_eq!(gpu(42.0), "42%");
    }

    #[test]
    fn exe_path_igual_que_metrics_js() {
        let larga = r"C:\Users\Breiner\AppData\Local\app.exe";
        assert_eq!(exe_path(Some(larga)), r"…\Local\app.exe");
        let corta = r"C:\app.exe";
        assert_eq!(exe_path(Some(corta)), corta);
        assert_eq!(exe_path(None), "ruta no disponible");
        assert_eq!(exe_path(Some("  ")), "ruta no disponible");
    }
}
