//! Icono del .exe (Explorer, acceso directo, Alt-Tab sin WM_SETICON):
//! compila app.rc con windres (MinGW, en PATH) a COFF y lo enlaza.
//! Sin crates nuevos: solo comandos del sistema.
//! app.ico es copia de ../src-tauri/icons/icon.ico (misma marca).
fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=app.ico");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    let res = format!("{out}\\app.res");
    let st = std::process::Command::new("windres.exe")
        .args(["app.rc", "-O", "coff", "-o", &res])
        .current_dir(&manifest)
        .status()
        .expect("windres.exe no encontrado en PATH (instalar MinGW)");
    assert!(st.success(), "windres no pudo compilar app.rc");
    println!("cargo:rustc-link-arg={res}");
}
