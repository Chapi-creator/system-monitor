//! Lib del crate Tauri: tauri-build genera el contexto aquí y el binario lo
//! consume. El núcleo real (estado, sampler, formateadores) vive en el crate
//! sysmon-core, compartido con el binario nativo (native/).

pub use sysmon_core::*;
