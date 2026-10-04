use j2534_0404::J2534Api0404;

#[test]
fn read_version_with_configured_dll_path() {
    let dll_path = match std::env::var("J2534_DLL_PATH") {
        Ok(path) => path,
        Err(_) => return,
    };

    let api =
        J2534Api0404::from_path(&dll_path).expect("J2534 DLL should load from J2534_DLL_PATH");
    let device_id = api
        .open(None)
        .expect("PassThruOpen should succeed with configured device");
    let version = api
        .read_version(device_id)
        .expect("PassThruReadVersion should return version strings");

    assert!(
        !version.api.trim().is_empty(),
        "API version string should not be empty"
    );

    api.close(device_id)
        .expect("PassThruClose should succeed after reading version");
}
