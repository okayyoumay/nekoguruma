#[cfg(feature = "protoc")]
use prost_build::Config;
#[cfg(feature = "protoc")]
use tonic_prost_build::configure;

fn main() {
    build_protobuf();
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
