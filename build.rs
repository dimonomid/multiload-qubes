fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "panel")]
    generate_panel_header();
}

// Generate the adapter's C declarations from the Rust ABI in panel builds only.
#[cfg(feature = "panel")]
fn generate_panel_header() {
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let header = root.join("build/panel-api.h");
    println!("cargo:rerun-if-changed=src/panel/ffi.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    // The C build consumes this published file. Watching it also regenerates
    // it if build/ is removed while Cargo's compiled artifacts remain cached.
    println!("cargo:rerun-if-changed=build/panel-api.h");
    std::fs::create_dir_all(header.parent().unwrap()).expect("create header directory");
    cbindgen::Builder::new()
        .with_src(root.join("src/panel/ffi.rs"))
        .with_config(
            cbindgen::Config::from_file(root.join("cbindgen.toml")).expect("read cbindgen config"),
        )
        .generate()
        .expect("generate XFCE bridge header")
        .write_to_file(header);
}
