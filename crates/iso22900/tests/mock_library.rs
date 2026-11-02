use iso22900::{ComLogicalLinkHandle, DPduApi, DPduApiError, ModuleHandle};

#[test]
fn mock_library_supports_basic_wrapper_calls() {
    let path = iso22900_mock::mock_library_path()
        .expect("mock library should be discoverable after build");
    let api = DPduApi::new(&path).expect("mock library should load");

    let modules = api.get_module_ids().expect("module ids should succeed");
    let entries = modules
        .borrowed()
        .entries()
        .expect("module entries should be readable");
    assert_eq!(entries.len(), 1);
    let module_handle = ModuleHandle(entries[0].module_handle());

    let version = api
        .get_version(module_handle)
        .expect("version should succeed");
    assert_eq!(version.hardware_name, "Mock Hardware");
    assert_eq!(version.api_software_name, "iso22900-mock");

    // The mock's module clock counts microseconds and only moves forward.
    let first = api
        .get_timestamp(module_handle)
        .expect("timestamp should succeed");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let second = api
        .get_timestamp(module_handle)
        .expect("timestamp should succeed");
    assert!(
        second.wrapping_sub(first) >= 2_000,
        "the module clock should advance by at least the 2 ms slept: {first} then {second}"
    );
}

#[test]
fn io_ctl_with_null_output_item_is_an_error() {
    let path = iso22900_mock::mock_library_path()
        .expect("mock library should be discoverable after build");
    let api = DPduApi::new(&path).expect("mock library should load");

    let result = api.io_ctl_with_output(
        ModuleHandle(1001),
        ComLogicalLinkHandle(3001),
        iso22900_mock::MOCK_IOCTL_NULL_OUTPUT,
        None,
    );
    assert!(matches!(result, Err(DPduApiError::NullPointer(_))));
}
