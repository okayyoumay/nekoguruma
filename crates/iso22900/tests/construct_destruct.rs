//! Repeated `PDUConstruct` / `PDUDestruct` cycles through `DPduApi` against
//! `iso22900-mock`: every `DPduApi::new` is paired with exactly one
//! `PDUDestruct` on drop, and nothing a dropped instance left behind
//! (registered callbacks, queued events) reaches the next instance.
//!
//! The mock's state is process-global and lives in the loaded cdylib, not in
//! the `iso22900_mock` rlib this test links, so the counters are read through
//! the library's own `__mock_*` exports. The tests in this file share that
//! state and take `SERIAL` first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use iso22900::{
    ComLogicalLinkHandle, ComPrimitiveControl, ComPrimitiveType, DPduApi, DPduApiError, E_PDU_COPT,
    E_PDU_ERROR, FlagData, ModuleHandle, Resource, ResourceId,
};
use iso22900_sys::bindings::T_PDU_ERROR;
use iso22900_sys::libloading::{Library, Symbol};

const CYCLES: usize = 25;

/// The mock's only resource (`crates/iso22900-mock/docs/testing-guide.md`).
const MOCK_RESOURCE_ID: u32 = 2001;

static SERIAL: Mutex<()> = Mutex::new(());

/// The mock library and its back-door exports. Loading the same path again
/// returns the library `DPduApi::new` loads, so both see the same state.
struct Mock {
    _serial: MutexGuard<'static, ()>,
    path: PathBuf,
    library: Library,
}

impl Mock {
    fn open() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let path = iso22900_mock::mock_library_path()
            .expect("mock library should be discoverable after build");
        let library = unsafe { Library::new(&path) }.expect("mock library should load");
        let mock = Self {
            _serial: serial,
            path,
            library,
        };
        mock.reset();
        mock
    }

    /// Calls a back-door export that takes no arguments. With no arguments
    /// the C and stdcall conventions agree, so one signature serves every
    /// target.
    fn call<R>(&self, name: &str) -> R {
        let symbol: Symbol<'_, unsafe extern "C" fn() -> R> =
            unsafe { self.library.get(name.as_bytes()) }
                .unwrap_or_else(|e| panic!("mock should export {name}: {e}"));
        unsafe { symbol() }
    }

    fn reset(&self) {
        let _: T_PDU_ERROR = self.call("__mock_reset");
    }

    fn construct_count(&self) -> usize {
        self.call("__mock_get_construct_count")
    }

    fn destruct_count(&self) -> usize {
        self.call("__mock_get_destruct_count")
    }

    fn api(&self) -> DPduApi {
        DPduApi::new(Path::new(&self.path)).expect("PDUConstruct should succeed")
    }
}

/// Connects the mock's only module and creates a link on its only resource.
fn open_link(api: &DPduApi) -> (ModuleHandle, ComLogicalLinkHandle) {
    let modules = api.get_module_ids().expect("module ids should succeed");
    let module = ModuleHandle(
        modules
            .borrowed()
            .entries()
            .expect("module entries should be readable")[0]
            .module_handle(),
    );
    drop(modules);
    api.module_connect(module)
        .expect("module connect should succeed");
    let link = api
        .create_com_logical_link(
            module,
            Resource::ById(ResourceId(MOCK_RESOURCE_ID)),
            None,
            FlagData::default(),
        )
        .expect("link creation should succeed");
    (module, link)
}

fn start_primitive(api: &DPduApi, module: ModuleHandle, link: ComLogicalLinkHandle) {
    api.start_com_primitive(
        module,
        link,
        ComPrimitiveType(E_PDU_COPT::PDU_COPT_SENDRECV),
        vec![0x3E, 0x00],
        ComPrimitiveControl {
            time: 0,
            send_cycles: 1,
            receive_cycles: 1,
            temp_param_update: 0,
            tx_flags: FlagData::default(),
            expected_responses: vec![],
        },
        0,
    )
    .expect("start primitive should succeed");
}

fn assert_queue_empty(api: &DPduApi, module: ModuleHandle, link: ComLogicalLinkHandle) {
    match api.get_event_item(module, link) {
        Err(DPduApiError::PduError(code)) => assert_eq!(
            code,
            E_PDU_ERROR::PDU_ERR_EVENT_QUEUE_EMPTY.0,
            "an earlier instance's events should not reach a new one"
        ),
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("an earlier instance's events should not reach a new one"),
    }
}

#[test]
fn every_construct_is_paired_with_one_destruct() {
    let mock = Mock::open();
    for cycle in 1..=CYCLES {
        let api = mock.api();
        assert_eq!(mock.construct_count(), cycle);
        assert_eq!(mock.destruct_count(), cycle - 1, "destruct before drop");
        let (module, _link) = open_link(&api);
        api.get_version(module).expect("version should succeed");
        drop(api);
        assert_eq!(mock.construct_count(), cycle);
        assert_eq!(mock.destruct_count(), cycle, "drop should call PDUDestruct");
    }
}

#[test]
fn a_dropped_instance_leaves_no_callback_or_event_behind() {
    let mock = Mock::open();
    let calls = Arc::new(AtomicUsize::new(0));
    for cycle in 1..=CYCLES {
        let api = mock.api();
        let (module, link) = open_link(&api);
        // Events queued by the previous instance and never read are gone.
        assert_queue_empty(&api, module, link);

        let counter = Arc::clone(&calls);
        api.register_event_callback(module, link, move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .expect("callback registration should succeed");
        start_primitive(&api, module, link);
        // Exactly this instance's one event: no callback of an earlier
        // instance fires, and registering replays nothing stale.
        assert_eq!(calls.load(Ordering::SeqCst), cycle);
        // The event stays queued unread when the instance is dropped.
        drop(api);
        assert_eq!(
            Arc::strong_count(&calls),
            1,
            "dropping the instance should free its registered callbacks"
        );
    }

    // An instance that registers nothing gets no callback from a dropped one.
    let api = mock.api();
    let (module, link) = open_link(&api);
    start_primitive(&api, module, link);
    assert_eq!(calls.load(Ordering::SeqCst), CYCLES);
    drop(api);
    assert_eq!(mock.construct_count(), mock.destruct_count());
}
