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
        .header("src/bindings/j2534_v0404.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .derive_debug(true)
        .dynamic_library_name("J2534Api0404")
        .dynamic_link_require_all(true)
        .allowlist_function("PassThru.*")
        .allowlist_type(
            "PASSTHRU_.*|SCONFIG.*|SBYTE_ARRAY|REPEAT_MSG_SETUP|SPARAM.*|NDIS_ADAPTER_INFORMATION",
        )
        .allowlist_var("^(STATUS_.*|ERR_.*|PROTOCOL_.*|CONFIG_.*|IOCTL_.*|CONNECT_FLAG_.*|TX_FLAG_.*|RX_FLAG_.*|DEVICE_INFO_.*|.*_FILTER)$");

    if target.ends_with("-pc-windows-gnullvm") {
        builder = builder.clang_arg(format!(
            "--target={}",
            target.replace("-pc-windows-gnullvm", "-pc-windows-gnu")
        ));
    }

    let bindings = builder.generate().expect("Unable to generate bindings");

    bindings
        .write_to_file(&bindings_file)
        .expect("Couldn't write bindings");
}

fn main() {
    println!("cargo:rerun-if-changed=src/bindings/j2534_v0404.h");
    run_bindgen();
    // LP64 targets (64-bit non-Windows) need the `unsigned long` = 8 conversion layer.
    println!("cargo::rustc-check-cfg=cfg(lp64)");
    let windows = std::env::var("CARGO_CFG_WINDOWS").is_ok();
    let width = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();
    if !windows && width == "64" {
        println!("cargo:rustc-cfg=lp64");
    }
}
