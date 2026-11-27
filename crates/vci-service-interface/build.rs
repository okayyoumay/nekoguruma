#[cfg(feature = "protoc")]
use prost_build::Config;
#[cfg(feature = "protoc")]
use tonic_prost_build::configure;

/// The variable that turns the regeneration on. The `protoc` feature alone builds the generator
/// but leaves the committed bindings as they are, so a `--all-features` check or Clippy run of the
/// workspace does not rewrite them.
const REGENERATE: &str = "NGR_REGENERATE_PROTO";

fn main() {
    println!("cargo:rerun-if-env-changed={REGENERATE}");
    if std::env::var_os(REGENERATE).is_some_and(|value| value == "1") {
        build_protobuf();
    }
}
#[cfg(not(feature = "protoc"))]
fn build_protobuf() {}

#[cfg(feature = "protoc")]
fn build_protobuf() {
    #[cfg(feature = "vendored-protoc")]
    let _include_path = protoc_bin_vendored::include_path()
        .unwrap()
        .as_os_str()
        .to_str()
        .unwrap();

    let mut config = Config::new();
    #[cfg(feature = "vendored-protoc")]
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path().unwrap());
    config.default_package_filename("service");

    configure()
        .emit_package(false)
        .out_dir("src/bindings")
        .build_client(true)
        .build_server(true)
        .compile_with_config(config, &["src/proto/service.proto"], &["src/proto"])
        .unwrap();
}
