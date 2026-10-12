//! Unit-test setup shared by the service's test modules: the mock library is found through a
//! test root description file named by the debug-only `NGR_PDU_API_ROOT_FILE` override
//! (ADR-282), and per-library settings come from a test `config.toml` named by the debug-only
//! `VCI_CONFIG_PATH` override (ADR-073). Neither names a `library_path`.

/// The library name the test root description file gives the mock.
pub(crate) const MOCK_LIBRARY_NAME: &str = "TestLib";

/// Writes a root description file naming the compiled `iso22900-mock` cdylib as
/// [`MOCK_LIBRARY_NAME`] and a `config.toml` holding `config_toml`, and points both overrides at
/// them. `tag` keeps the files of different test modules apart; the process id keeps two test
/// processes apart.
///
/// Callers must be `#[serial]`: this rewrites shared files and sets process-global environment
/// variables.
pub(crate) fn use_mock_library(tag: &str, config_toml: &str) {
    let library =
        iso22900_mock::mock_library_path().expect("mock cdylib should be discoverable after build");
    let dir = std::env::temp_dir().join(format!("iso22900-service-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("test directory should be creatable");
    let root_file = iso22900_mock::write_root_file(&dir, MOCK_LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config_toml).expect("config file should be writable");
    // SAFETY: the callers are `#[serial]`, so no other test reads the environment meanwhile.
    unsafe {
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
    }
}
