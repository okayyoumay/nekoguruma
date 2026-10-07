fn main() {
    // Locations of the files that decide which library a worker loads, fixed at build time
    // (ADR-228). Each is relative to the selected root (`/etc` on Linux, the selected Windows
    // known folder, or the executable's directory with `config-root-exe-dir`), or absolute to
    // bypass root resolution entirely.
    //
    // VCI_CONFIG_PATH: the worker services' configuration file.
    // Default: "nekoguruma/config.toml"
    let path =
        std::env::var("VCI_CONFIG_PATH").unwrap_or_else(|_| "nekoguruma/config.toml".to_owned());
    println!("cargo:rustc-env=VCI_CONFIG_PATH={path}");
    println!("cargo:rerun-if-env-changed=VCI_CONFIG_PATH");
    // NGR_J2534_DEFINITION_DIR: the J2534 registration definitions on Linux (design 7.1.1).
    // Default: "nekoguruma/j2534"
    let dir =
        std::env::var("NGR_J2534_DEFINITION_DIR").unwrap_or_else(|_| "nekoguruma/j2534".to_owned());
    println!("cargo:rustc-env=NGR_J2534_DEFINITION_DIR={dir}");
    println!("cargo:rerun-if-env-changed=NGR_J2534_DEFINITION_DIR");
}
