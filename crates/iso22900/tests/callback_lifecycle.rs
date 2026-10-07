//! Event-callback lifecycle of `DPduApi` against `iso22900-mock`: what
//! registering, re-registering and unregistering a callback, destroying its
//! link and disconnecting its module do to delivery and to the stored
//! closure, and that registration racing with event delivery on another
//! thread neither deadlocks nor delivers to a removed callback.
//!
//! The mock calls the registered native callback synchronously from
//! `PDUStartComPrimitive` and replays every still-queued event when a callback
//! is registered, so the tests drain the queue after each primitive to keep
//! the counts exact. Its state is process-global and lives in the loaded
//! cdylib, so it is reset through the library's own `__mock_reset` export, and
//! the tests in this file take `SERIAL` first.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

use iso22900::{
    ComLogicalLinkHandle, ComPrimitiveControl, ComPrimitiveType, DPduApi, E_PDU_COPT, FlagData,
    ModuleHandle, Resource, ResourceId,
};
use iso22900_sys::bindings::T_PDU_ERROR;
use iso22900_sys::libloading::{Library, Symbol};

/// The mock's only resource (`crates/iso22900-mock/docs/testing-guide.md`).
const MOCK_RESOURCE_ID: u32 = 2001;

/// Register/unregister and start/drain rounds in the race test.
const RACE_ROUNDS: usize = 200;

/// How long a step that should finish may take before the test fails, so a
/// deadlock fails the test instead of hanging the CI job.
const DEADLINE: Duration = Duration::from_secs(60);

/// How long the test watches a step that must stay blocked.
const STILL_BLOCKED: Duration = Duration::from_millis(200);

static SERIAL: Mutex<()> = Mutex::new(());

/// A fresh mock with one connected module and one link, held for the test.
/// Fields drop in declaration order: the API is destructed before the extra
/// library handle is released and before the next test may reset the mock.
struct Fixture {
    api: Arc<DPduApi>,
    module: ModuleHandle,
    link: ComLogicalLinkHandle,
    _library: Library,
    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let path: PathBuf = iso22900_mock::mock_library_path()
            .expect("mock library should be discoverable after build");
        // Loading the same path again returns the library `DPduApi::new`
        // loads, so the reset reaches the state the API sees.
        let library = unsafe { Library::new(&path) }.expect("mock library should load");
        // The mock exports its back doors as stdcall on Windows x86 and as C
        // elsewhere, which is what `extern "system"` names on each target.
        let reset: Symbol<'_, unsafe extern "system" fn() -> T_PDU_ERROR> =
            unsafe { library.get(b"__mock_reset") }.expect("mock should export __mock_reset");
        unsafe { reset() };

        let api = DPduApi::new(&path).expect("PDUConstruct should succeed");
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
        Self {
            api: Arc::new(api),
            module,
            link,
            _library: library,
            _serial: serial,
        }
    }

    /// Registers a callback that counts its calls in `calls`.
    fn register(&self, calls: &Arc<AtomicUsize>) {
        register_counter(&self.api, self.module, self.link, calls);
    }

    /// Starts a primitive (the mock reports one event for it at once) and
    /// reads every queued event, so a later registration replays none.
    fn start_and_drain(&self) {
        start_and_drain(&self.api, self.module, self.link);
    }
}

fn register_counter(
    api: &DPduApi,
    module: ModuleHandle,
    link: ComLogicalLinkHandle,
    calls: &Arc<AtomicUsize>,
) {
    let counter = Arc::clone(calls);
    api.register_event_callback(module, link, move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    })
    .expect("callback registration should succeed");
}

/// Starts a primitive; the mock reports one event for it at once.
fn start(api: &DPduApi, module: ModuleHandle, link: ComLogicalLinkHandle) {
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

fn start_and_drain(api: &DPduApi, module: ModuleHandle, link: ComLogicalLinkHandle) {
    start(api, module, link);
    while api.get_event_item(module, link).is_ok() {}
}

fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn calls(counter: &Arc<AtomicUsize>) -> usize {
    counter.load(Ordering::SeqCst)
}

#[test]
fn unregistering_stops_delivery_and_frees_the_callback() {
    let f = Fixture::new();
    let a = counter();
    f.register(&a);
    f.start_and_drain();
    assert_eq!(calls(&a), 1);

    f.api
        .unregister_event_callback(f.module, f.link)
        .expect("unregistering should succeed");
    assert_eq!(Arc::strong_count(&a), 1, "the closure should be dropped");
    f.start_and_drain();
    assert_eq!(calls(&a), 1, "no delivery after unregistering");

    // Unregistering again, with nothing registered, is harmless.
    f.api
        .unregister_event_callback(f.module, f.link)
        .expect("a second unregister should succeed");
}

#[test]
fn registering_again_replaces_the_callback() {
    let f = Fixture::new();
    let a = counter();
    let b = counter();
    f.register(&a);
    f.register(&b);
    assert_eq!(Arc::strong_count(&a), 1, "the replaced closure is dropped");
    f.start_and_drain();
    assert_eq!((calls(&a), calls(&b)), (0, 1));
}

#[test]
fn a_late_registration_receives_the_events_already_queued() {
    let f = Fixture::new();
    // Two events queued before any callback exists.
    for _ in 0..2 {
        start(&f.api, f.module, f.link);
    }
    // The closure is stored before the native registration, so the events
    // the library reports during it reach the closure.
    let a = counter();
    f.register(&a);
    assert_eq!(calls(&a), 2);
}

#[test]
fn destroying_the_link_frees_its_callback() {
    let f = Fixture::new();
    let a = counter();
    f.register(&a);
    f.api
        .destroy_com_logical_link(f.module, f.link)
        .expect("destroying the link should succeed");
    assert_eq!(Arc::strong_count(&a), 1, "the closure should be dropped");
    // The mock still reports events for the old handle; nobody receives them.
    f.start_and_drain();
    assert_eq!(calls(&a), 0);
}

#[test]
fn disconnecting_the_module_frees_its_callbacks() {
    let f = Fixture::new();
    let a = counter();
    f.register(&a);
    f.api
        .module_disconnect(f.module)
        .expect("module disconnect should succeed");
    assert_eq!(Arc::strong_count(&a), 1, "the closure should be dropped");
    f.start_and_drain();
    assert_eq!(calls(&a), 0);
}

/// Holds a callback inside its delivery on one thread while another thread
/// unregisters it: the unregister must wait for the callback to return, and
/// only then drop the closure, so a closure is never freed while it runs.
///
/// The callback stays blocked until the unregistering thread has reported
/// that it is calling `unregister_event_callback`, and for `STILL_BLOCKED`
/// after that, so the unregister has reached the callback registry's lock
/// before the callback returns unless that thread is descheduled between the
/// report and the lock for the whole window. Proving that it waits at the
/// lock itself would need a hook inside the wrapper; the assertions hold
/// either way, so the test never fails spuriously.
#[test]
fn unregistering_during_a_delivery_waits_for_the_callback_to_return() {
    let f = Fixture::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let alive = Arc::new(());
    let in_callback = Arc::clone(&alive);
    f.api
        .register_event_callback(f.module, f.link, move |_| {
            let _keep = &in_callback;
            entered_tx.send(()).expect("the test should be waiting");
            release_rx
                .recv_timeout(DEADLINE)
                .expect("the test should release the callback");
        })
        .expect("callback registration should succeed");

    let deliverer = {
        let api = Arc::clone(&f.api);
        let (module, link) = (f.module, f.link);
        thread::spawn(move || start(&api, module, link))
    };
    entered_rx
        .recv_timeout(DEADLINE)
        .expect("the callback should be called");

    let (calling_tx, calling_rx) = mpsc::channel();
    let (unregistered_tx, unregistered_rx) = mpsc::channel();
    let unregisterer = {
        let api = Arc::clone(&f.api);
        let (module, link) = (f.module, f.link);
        thread::spawn(move || {
            calling_tx.send(()).expect("the test should be waiting");
            api.unregister_event_callback(module, link)
                .expect("unregistering should succeed");
            unregistered_tx
                .send(())
                .expect("the test should be waiting");
        })
    };
    calling_rx
        .recv_timeout(DEADLINE)
        .expect("the unregistering thread should start");
    assert!(
        unregistered_rx.recv_timeout(STILL_BLOCKED).is_err(),
        "unregistering should wait while the callback runs"
    );
    assert_eq!(Arc::strong_count(&alive), 2, "the running closure is kept");

    release_tx.send(()).expect("the callback should be waiting");
    unregistered_rx
        .recv_timeout(DEADLINE)
        .expect("unregistering should finish once the callback returns");
    deliverer
        .join()
        .expect("the delivering thread should not panic");
    unregisterer
        .join()
        .expect("the unregistering thread should not panic");
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "the closure should be dropped"
    );
}

/// Stress coverage on top of the forced interleaving above: both threads
/// start together and run many rounds.
#[test]
fn registration_racing_with_event_delivery_neither_deadlocks_nor_leaks() {
    let f = Fixture::new();
    let a = counter();
    let (done_tx, done_rx) = mpsc::channel();
    let start_together = Arc::new(Barrier::new(2));

    let registrar = {
        let api = Arc::clone(&f.api);
        let (module, link, a) = (f.module, f.link, Arc::clone(&a));
        let done = done_tx.clone();
        let barrier = Arc::clone(&start_together);
        thread::spawn(move || {
            barrier.wait();
            for _ in 0..RACE_ROUNDS {
                register_counter(&api, module, link, &a);
                api.unregister_event_callback(module, link)
                    .expect("unregistering should succeed");
            }
            done.send(()).expect("the test should be waiting");
        })
    };
    let deliverer = {
        let api = Arc::clone(&f.api);
        let (module, link) = (f.module, f.link);
        let barrier = Arc::clone(&start_together);
        thread::spawn(move || {
            barrier.wait();
            for _ in 0..RACE_ROUNDS {
                start_and_drain(&api, module, link);
            }
            done_tx.send(()).expect("the test should be waiting");
        })
    };
    for _ in 0..2 {
        // A deadlock between the callback map and the library would hang
        // here, so a lost wake-up fails the test instead of the CI job.
        done_rx
            .recv_timeout(DEADLINE)
            .expect("both threads should finish without deadlocking");
    }
    registrar
        .join()
        .expect("the registering thread should not panic");
    deliverer
        .join()
        .expect("the delivering thread should not panic");

    // Every registration ended with an unregister, so no closure is left and
    // nothing is delivered any more.
    assert_eq!(Arc::strong_count(&a), 1, "no closure should be left behind");
    let delivered = calls(&a);
    f.start_and_drain();
    assert_eq!(
        calls(&a),
        delivered,
        "no delivery after the last unregister"
    );
}
