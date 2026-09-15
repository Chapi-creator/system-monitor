//! Kill de procesos compartido por ambos binarios (Tauri y nativo).
//!
//! Validación pura (testeable) + ejecución vía `taskkill /F /T` que corta
//! árboles de proceso (process.kill de Rust no lo hace en Windows).

/// Validación equivalente a validatePid() del renderer: entero positivo,
/// distinto del propio widget y no crítico del sistema (PID 4 = System).
pub fn validate_kill_pid(pid: Option<i64>) -> Result<i64, &'static str> {
    let Some(pid) = pid.filter(|p| *p > 0) else {
        return Err("Invalid PID");
    };
    if pid as u32 == std::process::id() {
        return Err("Refusing to kill the widget itself");
    }
    // PID 4 = System (Windows): núcleo del SO, nunca matable desde la IU.
    #[cfg(windows)]
    if pid == 4 {
        return Err("Refusing to kill a system-critical process");
    }
    Ok(pid)
}

/// Ejecuta el kill vía taskkill. En creación de procesos usa
/// CREATE_NO_WINDOW para no parpadear una consola.
#[cfg(windows)]
pub fn kill_process(pid: Option<i64>) -> Result<i64, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let pid = validate_kill_pid(pid).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Failed to kill process {pid} ({e})"))?;
    if out.status.success() {
        Ok(pid)
    } else {
        Err(format!(
            "Failed to kill process {}: {}",
            pid,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(not(windows))]
pub fn kill_process(_pid: Option<i64>) -> Result<i64, String> {
    Err("Unsupported platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rechaza_pids_peligrosos_sin_ejecutar_nada() {
        assert_eq!(validate_kill_pid(None), Err("Invalid PID"));
        assert_eq!(validate_kill_pid(Some(0)), Err("Invalid PID"));
        assert_eq!(validate_kill_pid(Some(-3)), Err("Invalid PID"));
        // El propio widget: nunca.
        let self_pid = std::process::id() as i64;
        assert_eq!(
            validate_kill_pid(Some(self_pid)),
            Err("Refusing to kill the widget itself")
        );
        #[cfg(windows)]
        assert_eq!(
            validate_kill_pid(Some(4)),
            Err("Refusing to kill a system-critical process")
        );
    }

    #[test]
    fn acepta_pids_validos() {
        assert_eq!(validate_kill_pid(Some(1234)), Ok(1234));
    }
}
