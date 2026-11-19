fn main() {
    if std::env::var_os("CARGO_FEATURE_MILLIDA_BRIDGE").is_none() {
        tauri_build::build()
    }
}
