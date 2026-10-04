fn main() {
    // Path to the config file: relative to the selected root, or absolute
    // to bypass root resolution entirely.
    // Override at build time by setting VCI_CONFIG_PATH.
    // Default: "vci-service-launcher/config.toml"
    let path = std::env::var("VCI_CONFIG_PATH")
        .unwrap_or_else(|_| "vci-service-launcher/config.toml".to_owned());
    println!("cargo:rustc-env=VCI_CONFIG_PATH={path}");
    println!("cargo:rerun-if-env-changed=VCI_CONFIG_PATH");
}
