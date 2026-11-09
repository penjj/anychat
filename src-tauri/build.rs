fn main() {
    // Cargo does not watch web assets, so editing frontend/* without touching
    // Rust would silently keep serving the previously embedded bundle.
    println!("cargo:rerun-if-changed=../frontend");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=capabilities");
    tauri_build::build()
}
