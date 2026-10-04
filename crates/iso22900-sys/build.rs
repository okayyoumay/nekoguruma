#[cfg(not(feature = "bindgen"))]
fn run_bindgen() {
    use std::fs;
    let target = std::env::var("TARGET").expect("TARGET env var not set");
    println!("cargo:rustc-env=BINDINGS_RS_FILENAME={target}.rs");
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR env var not set");
    let bindings_file = format!("{manifest_dir}/src/bindings/{target}.rs");
    println!("cargo:rerun-if-changed={bindings_file}");
    if fs::metadata(&bindings_file).is_err() {
        panic!(
            "Bindings file {bindings_file} does not exist. Please enable the 'bindgen' feature to generate it."
        );
    }
}

#[cfg(feature = "bindgen")]
fn run_bindgen() {
    let target = std::env::var("TARGET").expect("TARGET env var not set");
    println!("cargo:rustc-env=BINDINGS_RS_FILENAME={target}.rs");
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR env var not set");
    let bindings_file = format!("{manifest_dir}/src/bindings/{target}.rs");
    println!("cargo:rerun-if-changed={bindings_file}");

    let mut builder = bindgen::Builder::default()
        //        .rust_edition(bindgen::RustEdition::Edition2024)
        .headers([
            "src/bindings/d_pdu_api_func.h",
            "src/bindings/d_pdu_api_defs.h",
        ])
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .derive_debug(true)
        .dynamic_library_name("DPduApiSys")
        .dynamic_link_require_all(true)
        .newtype_enum("E_PDU_.*")
        .allowlist_function("PDU.*")
        .allowlist_var("PDU.*")
        .allowlist_type("([ET]_)?PDU.*")
        .derive_debug(true);

    if target.ends_with("-pc-windows-gnullvm") {
        builder = builder.clang_arg(format!(
            "--target={}",
            target.replace("-pc-windows-gnullvm", "-pc-windows-gnu")
        ));
    }

    let bindings = builder.generate().expect("Unable to generate bindings");

    bindings
        .write_to_file(&bindings_file)
        .expect("Couldn't write bindings!");
}

fn main() {
    println!("cargo:rerun-if-changed=src/bindings/d_pdu_api_defs.h");
    println!("cargo:rerun-if-changed=src/bindings/d_pdu_api_func.h");

    run_bindgen();
}
