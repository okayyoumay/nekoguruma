fn main() {
    // Locations of the files that decide which library a worker loads, fixed at build time
    // (ADR-228). An empty value counts as unset.
    //
    // VCI_CONFIG_PATH: the worker services' configuration file, relative to the selected root
    // (`/etc` on Linux, the selected Windows known folder, or the executable's directory with
    // `config-root-exe-dir`) or absolute to bypass root resolution entirely.
    // Default: "nekoguruma/config.toml"
    let path =
        non_empty_var("VCI_CONFIG_PATH").unwrap_or_else(|| "nekoguruma/config.toml".to_owned());
    println!("cargo:rustc-env=VCI_CONFIG_PATH={path}");
    println!("cargo:rerun-if-env-changed=VCI_CONFIG_PATH");
    // NGR_J2534_DEFINITION_DIR: the J2534 registration definitions on Linux (design 7.1.1),
    // relative to the fixed system directory (`/etc` on Linux; no `config-root-*` feature
    // moves it) or absolute.
    // Default: "nekoguruma/j2534"
    let dir =
        non_empty_var("NGR_J2534_DEFINITION_DIR").unwrap_or_else(|| "nekoguruma/j2534".to_owned());
    println!("cargo:rustc-env=NGR_J2534_DEFINITION_DIR={dir}");
    println!("cargo:rerun-if-env-changed=NGR_J2534_DEFINITION_DIR");
    // NGR_PDU_API_ROOT_FILE: the D-PDU API root description file on Linux (design 7.1,
    // ISO 22900-2:2022 clause 8.7), relative to the same fixed system directory or absolute.
    // Default: "pdu_api_root.xml"
    let root_file =
        non_empty_var("NGR_PDU_API_ROOT_FILE").unwrap_or_else(|| "pdu_api_root.xml".to_owned());
    println!("cargo:rustc-env=NGR_PDU_API_ROOT_FILE={root_file}");
    println!("cargo:rerun-if-env-changed=NGR_PDU_API_ROOT_FILE");
}

fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
