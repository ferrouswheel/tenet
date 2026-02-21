fn main() {
    let udl_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("tenet_ffi.udl");
    uniffi::generate_scaffolding(udl_path.to_str().unwrap()).unwrap();
}
