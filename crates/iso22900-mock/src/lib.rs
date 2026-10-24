#![allow(clippy::too_many_arguments)]
#![allow(unsafe_code)]
#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iso22900_sys::bindings::*;
use serde::Deserialize;
use sim_ecu::{Addressing, EcuConfig, Fault, SimEcu};

const MOCK_MODULE_HANDLE: UNUM32 = 1001;
const MOCK_RESOURCE_ID: UNUM32 = 2001;
/// The ISO 22900-2 (2022) ComParams (Annex B.3, B.5) the mock resolves, with the object ids it
/// gives them: those the agent sets up a UDS on CAN link with, the block size and minimum
/// separation time of ISO 15765-2 (2024) 9.6.5.3 and 9.6.5.4 the example ECU declares (ADR-324),
/// and those the hardware-check guard adds when it completes a job's link from its profile
/// (addressing modes, the functional identifier, formats, address bytes, filler and the
/// tester-present message; ADR-334). The mock keeps no ComParam values.
pub const STANDARD_COMPARAMS: [(&str, UNUM32); 26] = [
    ("CP_Baudrate", 3210),
    ("CP_P2Max", 3211),
    ("CP_P2Star", 3212),
    ("CP_RC78Handling", 3213),
    ("CP_RCByteOffset", 3214),
    ("CP_RC78CompletionTimeout", 3215),
    ("CP_CanPhysReqId", 3216),
    ("CP_CanRespUSDTId", 3217),
    ("CP_BlockSize", 3218),
    ("CP_StMin", 3219),
    ("CP_RC21Handling", 3220),
    ("CP_RC23Handling", 3221),
    ("CP_TransmitIndEnable", 3222),
    ("CP_StartMsgIndEnable", 3223),
    ("CP_CanFuncReqId", 3224),
    ("CP_RequestAddrMode", 3225),
    ("CP_TesterPresentAddrMode", 3226),
    ("CP_CanPhysReqFormat", 3227),
    ("CP_CanRespUSDTFormat", 3228),
    ("CP_CanFuncReqFormat", 3229),
    ("CP_CanPhysReqExtAddr", 3230),
    ("CP_CanRespUSDTExtAddr", 3231),
    ("CP_CanFuncReqExtAddr", 3232),
    ("CP_CanFillerByteHandling", 3233),
    ("CP_CanFillerByte", 3234),
    ("CP_TesterPresentMessage", 3235),
];

fn standard_comparam_id(name: &str) -> Option<UNUM32> {
    STANDARD_COMPARAMS
        .iter()
        .find(|(short_name, _)| *short_name == name)
        .map(|&(_, id)| id)
}

/// Object ids the mock gives the ISO 22900-2 (2022) short names it resolves (Annex B.1, B.2).
pub const STANDARD_BUS_TYPE_DWCAN: UNUM32 = 3111;
pub const STANDARD_PROTOCOL_UDS_ON_CAN: UNUM32 = 3112;
pub const STANDARD_PIN_TYPE_HI: UNUM32 = 3311;
pub const STANDARD_PIN_TYPE_LOW: UNUM32 = 3312;
const MOCK_LOGICAL_LINK_HANDLE: UNUM32 = 3001;
const MOCK_COM_PRIMITIVE_HANDLE: UNUM32 = 4001;
const MOCK_TIMESTAMP: UNUM32 = 4242;
const MOCK_VERSION_TAG: &[u8] = b"iso22900-mock\0";
const MOCK_VEHICLE_NAME: &[u8] = b"mock-vehicle\0";

/// The mock's additional error code (ISO 22900-2:2022 D.4.2, `ExtraErrorInfoId`) on the receive
/// timeout of a ComPrimitive the attached ECU did not answer. D.4.2 leaves these codes to the
/// vendor's MDF, so the value is the mock's own.
pub const MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP: UNUM32 = 0x0000_0101;

/// IOCTL command id for which `PDUIoCtl` reports success without filling in the output item,
/// so tests can check how the wrapper handles a library that returns a null item.
pub const MOCK_IOCTL_NULL_OUTPUT: UNUM32 = 0x7FFF_FF00;

/// Environment variable naming a JSON file with the `sim_ecu::EcuConfig` of a simulated ECU to
/// attach when `PDUConstruct` is called, as `sim-vci` reads it (ADR-299).
pub const ECU_CONFIG_ENV: &str = "NGR_SIM_ECU_CONFIG";

#[derive(Clone)]
struct PendingEvent {
    h_mod: UNUM32,
    h_cll: UNUM32,
    h_cop: UNUM32,
    tag: usize,
    timestamp: UNUM32,
    kind: EventKind,
}

/// What an event item carries.
#[derive(Clone)]
enum EventKind {
    /// A result item with these data bytes (none for the synthetic completion event).
    Result(Vec<u8>),
    /// A ComPrimitive status item.
    Status(T_PDU_STATUS),
    /// An error item, with its additional error code.
    Error(T_PDU_ERR_EVT, UNUM32),
}

#[derive(Default, Clone, Copy)]
struct CallCounters {
    construct: usize,
    destruct: usize,
    get_version: usize,
    get_status: usize,
    create_com_logical_link: usize,
    destroy_com_logical_link: usize,
    connect: usize,
    disconnect: usize,
    start_com_primitive: usize,
    get_event_item: usize,
}

/// The response `PDUGetLastError` reports on every call, settable via
/// `__mock_set_last_error`. Defaults to `PDU_ERR_EVT_NOERROR`/0/0/0, matching
/// this mock's always-no-error behavior prior to the ADR-105 P2 backlog fix
/// that made it configurable.
#[derive(Clone, Copy)]
struct LastErrorReport {
    error_code: T_PDU_ERR_EVT,
    cop_handle: UNUM32,
    timestamp: UNUM32,
    extra_error_info: UNUM32,
}

impl Default for LastErrorReport {
    fn default() -> Self {
        Self {
            error_code: E_PDU_ERR_EVT::PDU_ERR_EVT_NOERROR,
            cop_handle: 0,
            timestamp: 0,
            extra_error_info: 0,
        }
    }
}

#[derive(Default)]
struct MockState {
    constructed: bool,
    api_tag: usize,
    callback: CALLBACKFNC,
    next_com_primitive_handle: UNUM32,
    pending_events: Vec<PendingEvent>,
    counters: CallCounters,
    /// Raw D-PDU error code `PDULockResource` returns instead of its normal
    /// success behavior, set via `__mock_set_lock_resource_error`. `None`
    /// (the default after `__mock_reset`) means no override --
    /// `PDULockResource` behaves normally. Lets `iso22900-service` tests
    /// exercise `with_api_for_link`/`map_runtime_error_for_link`'s
    /// CLL-scoped native-failure path (ADR-105 P2 backlog) without a real
    /// adapter, mirroring `j2534-0404-mock`'s `prog_voltage_error`
    /// convention.
    lock_resource_error: Option<T_PDU_ERROR>,
    /// The response `PDUGetLastError` reports on every call. See
    /// [`LastErrorReport`].
    last_error_report: LastErrorReport,
    /// The simulated ECU that answers `PDU_COPT_SENDRECV` ComPrimitives, if one is attached
    /// (`__mock_sim_ecu_control`, or `NGR_SIM_ECU_CONFIG` at `PDUConstruct`; ADR-299).
    ecu: Option<SimEcu>,
    /// ComPrimitives started while an ECU is attached (or while others still wait or run),
    /// waiting for the one before them to finish, in start order. The delivery thread
    /// ([`deliver`]) begins each in turn. Discarded with the event queue (`PDUConstruct`,
    /// `PDUDestruct`, a reset).
    waiting: VecDeque<WaitingCop>,
    /// Events of the running ComPrimitive, with the time each is due, in due order. The delivery
    /// thread moves each to `pending_events` when it is due. Discarded with the event queue.
    scheduled: VecDeque<(Instant, PendingEvent)>,
    /// Set by `PDUDestruct` to end the delivery thread.
    stop_delivery: bool,
}

impl MockState {
    fn next_primitive_handle(&mut self) -> UNUM32 {
        if self.next_com_primitive_handle == 0 {
            self.next_com_primitive_handle = MOCK_COM_PRIMITIVE_HANDLE;
        }

        let handle = self.next_com_primitive_handle;
        self.next_com_primitive_handle = self.next_com_primitive_handle.saturating_add(1);
        handle
    }
}

fn state() -> &'static Mutex<MockState> {
    static STATE: OnceLock<Mutex<MockState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(MockState::default()))
}

fn no_error() -> T_PDU_ERROR {
    E_PDU_ERROR::PDU_STATUS_NOERROR
}

fn invalid_parameters() -> T_PDU_ERROR {
    E_PDU_ERROR::PDU_ERR_INVALID_PARAMETERS
}

fn event_queue_empty() -> T_PDU_ERROR {
    E_PDU_ERROR::PDU_ERR_EVENT_QUEUE_EMPTY
}

unsafe fn set_out<T: Copy>(ptr: *mut T, value: T) -> T_PDU_ERROR {
    if ptr.is_null() {
        invalid_parameters()
    } else {
        unsafe {
            *ptr = value;
        }
        no_error()
    }
}

unsafe fn set_out_ptr<T>(ptr: *mut *mut T, value: *mut T) -> T_PDU_ERROR {
    if ptr.is_null() {
        invalid_parameters()
    } else {
        unsafe {
            *ptr = value;
        }
        no_error()
    }
}

fn fill_fixed_string(dst: &mut [CHAR8], value: &str) {
    for byte in dst.iter_mut() {
        *byte = 0;
    }

    let bytes = value.as_bytes();
    let copy_len = bytes.len().min(dst.len().saturating_sub(1));
    for index in 0..copy_len {
        dst[index] = bytes[index] as CHAR8;
    }
}

fn fixed_version() -> PDU_VERSION_DATA {
    let mut data = unsafe { std::mem::zeroed::<PDU_VERSION_DATA>() };
    data.MVCI_Part1StandardVersion = 1;
    data.MVCI_Part2StandardVersion = 0;
    data.HwSerialNumber = 123456;
    fill_fixed_string(&mut data.HwName, "Mock Hardware");
    data.HwVersion = 7;
    data.HwDate = 20260519;
    data.HwInterface = 1;
    fill_fixed_string(&mut data.FwName, "Mock Firmware");
    data.FwVersion = 11;
    data.FwDate = 20260519;
    fill_fixed_string(&mut data.VendorName, "Mock Vendor");
    fill_fixed_string(&mut data.PDUApiSwName, "iso22900-mock");
    data.PDUApiSwVersion = 42;
    data.PDUApiSwDate = 20260519;
    data
}

unsafe fn make_module_item() -> *mut PDU_MODULE_ITEM {
    let mut module_data = vec![PDU_MODULE_DATA {
        ModuleTypeId: 9001,
        hMod: MOCK_MODULE_HANDLE,
        pVendorModuleName: MOCK_VERSION_TAG.as_ptr() as *mut CHAR8,
        pVendorAdditionalInfo: MOCK_VEHICLE_NAME.as_ptr() as *mut CHAR8,
        ModuleStatus: E_PDU_STATUS::PDU_MODST_AVAIL,
    }];

    let module_data_ptr = module_data.as_mut_ptr();
    std::mem::forget(module_data);

    Box::into_raw(Box::new(PDU_MODULE_ITEM {
        ItemType: E_PDU_IT::PDU_IT_MODULE_ID,
        NumEntries: 1,
        pModuleData: module_data_ptr,
    }))
}

unsafe fn make_resource_id_item(module_handle: UNUM32) -> *mut PDU_RSC_ID_ITEM {
    let mut resource_ids = vec![MOCK_RESOURCE_ID];
    let resource_ids_ptr = resource_ids.as_mut_ptr();
    std::mem::forget(resource_ids);

    let mut resource_id_data = vec![PDU_RSC_ID_ITEM_DATA {
        hMod: module_handle,
        NumIds: 1,
        pResourceIdArray: resource_ids_ptr,
    }];

    let resource_data_ptr = resource_id_data.as_mut_ptr();
    std::mem::forget(resource_id_data);

    Box::into_raw(Box::new(PDU_RSC_ID_ITEM {
        ItemType: E_PDU_IT::PDU_IT_RSC_ID,
        NumModules: 1,
        pResourceIdDataArray: resource_data_ptr,
    }))
}

unsafe fn make_conflict_item() -> *mut PDU_RSC_CONFLICT_ITEM {
    let mut conflict_data = vec![PDU_RSC_CONFLICT_DATA {
        hMod: MOCK_MODULE_HANDLE,
        ResourceId: MOCK_RESOURCE_ID,
    }];

    let conflict_ptr = conflict_data.as_mut_ptr();
    std::mem::forget(conflict_data);

    Box::into_raw(Box::new(PDU_RSC_CONFLICT_ITEM {
        ItemType: E_PDU_IT::PDU_IT_RSC_CONFLICT,
        NumEntries: 1,
        pRscConflictData: conflict_ptr,
    }))
}

unsafe fn make_unique_resp_table_item() -> *mut PDU_UNIQUE_RESP_ID_TABLE_ITEM {
    let mut unique_data = vec![PDU_ECU_UNIQUE_RESP_DATA {
        UniqueRespIdentifier: 1,
        NumParamItems: 0,
        pParams: ptr::null_mut(),
    }];

    let unique_ptr = unique_data.as_mut_ptr();
    std::mem::forget(unique_data);

    Box::into_raw(Box::new(PDU_UNIQUE_RESP_ID_TABLE_ITEM {
        ItemType: E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE,
        NumEntries: 1,
        pUniqueData: unique_ptr,
    }))
}

unsafe fn make_param_item(param_id: UNUM32) -> *mut PDU_PARAM_ITEM {
    let value = Box::new(0u32);
    let value_ptr = Box::into_raw(value).cast::<c_void>();

    Box::into_raw(Box::new(PDU_PARAM_ITEM {
        ItemType: E_PDU_IT::PDU_IT_PARAM,
        ComParamId: param_id,
        ComParamDataType: E_PDU_PT::PDU_PT_UNUM32,
        ComParamClass: E_PDU_PC::PDU_PC_COM,
        pComParamData: value_ptr,
    }))
}

unsafe fn make_event_item(event: PendingEvent) -> *mut PDU_EVENT_ITEM {
    let (item_type, data_ptr) = match event.kind {
        EventKind::Result(bytes) => {
            let num_data_bytes = bytes.len() as UNUM32;
            let data_bytes = if bytes.is_empty() {
                ptr::null_mut()
            } else {
                Box::into_raw(bytes.into_boxed_slice()).cast::<UNUM8>()
            };
            let result_data = Box::new(PDU_RESULT_DATA {
                RxFlag: PDU_FLAG_DATA {
                    NumFlagBytes: 0,
                    pFlagData: ptr::null_mut(),
                },
                UniqueRespIdentifier: 1,
                AcceptanceId: 1,
                TimestampFlags: PDU_FLAG_DATA {
                    NumFlagBytes: 0,
                    pFlagData: ptr::null_mut(),
                },
                TxMsgDoneTimestamp: event.timestamp,
                StartMsgTimestamp: event.timestamp,
                pExtraInfo: ptr::null_mut(),
                NumDataBytes: num_data_bytes,
                pDataBytes: data_bytes,
            });
            (
                E_PDU_IT::PDU_IT_RESULT,
                Box::into_raw(result_data).cast::<c_void>(),
            )
        }
        EventKind::Status(status) => (
            E_PDU_IT::PDU_IT_STATUS,
            Box::into_raw(Box::new(status)).cast::<c_void>(),
        ),
        EventKind::Error(code, extra) => (
            E_PDU_IT::PDU_IT_ERROR,
            Box::into_raw(Box::new(PDU_ERROR_DATA {
                ErrorCodeId: code,
                ExtraErrorInfoId: extra,
            }))
            .cast::<c_void>(),
        ),
    };
    Box::into_raw(Box::new(PDU_EVENT_ITEM {
        ItemType: item_type,
        hCop: event.h_cop,
        pCoPTag: event.tag as *mut c_void,
        Timestamp: event.timestamp,
        pData: data_ptr,
    }))
}

unsafe fn notify_data_available(callback: CALLBACKFNC, event: &PendingEvent, api_tag: usize) {
    if let Some(callback) = callback {
        unsafe {
            callback(
                E_PDU_EVT_DATA::PDU_EVT_DATA_AVAILABLE,
                event.h_mod,
                event.h_cll,
                event.tag as *mut c_void,
                api_tag as *mut c_void,
            );
        }
    }
}

unsafe fn queue_event(event: PendingEvent) {
    let notified = event.clone();
    let (callback, api_tag) = {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.pending_events.push(event);
        (guard.callback, guard.api_tag)
    };

    unsafe { notify_data_available(callback, &notified, api_tag) };
}

/// Wakes the delivery thread when `MockState::waiting`, `MockState::scheduled` or
/// `MockState::stop_delivery` changes.
fn delivery_signal() -> &'static Condvar {
    static SIGNAL: Condvar = Condvar::new();
    &SIGNAL
}

/// The delivery thread, while one runs.
fn delivery_thread() -> &'static Mutex<Option<JoinHandle<()>>> {
    static THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
    &THREAD
}

/// A ComPrimitive that waits for the ones before it to finish (ADR-299).
struct WaitingCop {
    /// Its synthetic completion event, the template of its other events.
    event: PendingEvent,
    /// The request and cycles of a send-and-receive ComPrimitive with request bytes, for the ECU.
    request: Option<(Vec<u8>, Cycles)>,
}

/// Begins the next waiting ComPrimitive at `now`: its request reaches the ECU only now, as a
/// link executes its non-cyclic ComPrimitives one after another (ISO 22900-2 (2022) 8.2.6.5),
/// and its events are scheduled from now on. Without an ECU (detached meanwhile) it gets the
/// synthetic completion event.
fn begin_next(state: &mut MockState, now: Instant) {
    let Some(cop) = state.waiting.pop_front() else {
        return;
    };
    let events = match (state.ecu.as_mut(), cop.request) {
        (Some(ecu), Some((request, cycles))) => ecu_events(ecu, &request, cycles, &cop.event),
        _ => vec![(0, cop.event)],
    };
    for (delay_ms, event) in events {
        let due = now + Duration::from_millis(delay_ms.into());
        state.scheduled.push_back((due, event));
    }
}

/// Starts the delivery thread unless one runs. Called with the state locked; neither the
/// delivery thread nor `stop_delivery_thread` takes the state lock while holding this one.
fn ensure_delivery_thread() {
    let mut thread = delivery_thread().lock().expect("delivery thread poisoned");
    if thread.as_ref().is_none_or(JoinHandle::is_finished) {
        *thread = Some(std::thread::spawn(deliver));
    }
}

/// Ends the delivery thread and waits for it, so no callback runs once `PDUDestruct` returns.
fn stop_delivery_thread(mut guard: MutexGuard<'_, MockState>) {
    guard.stop_delivery = true;
    guard.waiting.clear();
    guard.scheduled.clear();
    delivery_signal().notify_all();
    drop(guard);
    let thread = delivery_thread()
        .lock()
        .expect("delivery thread poisoned")
        .take();
    let Some(thread) = thread else {
        state().lock().expect("mock state poisoned").stop_delivery = false;
        return;
    };
    if is_current_thread(&thread) {
        // `PDUDestruct` from inside the callback, where ISO 22900-2 (2022) permits only
        // `PDUGetEventItem` (8.2.4.2, 8.4.22.1): the thread cannot wait for itself, so it keeps
        // the stop flag and ends once the callback returns.
        *delivery_thread().lock().expect("delivery thread poisoned") = Some(thread);
        return;
    }
    let _ = thread.join();
    state().lock().expect("mock state poisoned").stop_delivery = false;
}

/// Whether the calling thread is `thread`, compared by the operating system's thread identity.
/// `std::thread::current` would leave a thread-local destructor of this library's own copy of
/// `std` on the caller's thread, which runs when that thread exits, after the caller may have
/// unloaded the library.
fn is_current_thread(thread: &JoinHandle<()>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::thread::{JoinHandleExt, RawPthread};
        unsafe extern "C" {
            fn pthread_self() -> RawPthread;
            fn pthread_equal(a: RawPthread, b: RawPthread) -> std::ffi::c_int;
        }
        unsafe { pthread_equal(pthread_self(), thread.as_pthread_t()) != 0 }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, RawHandle};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetThreadId(thread: RawHandle) -> u32;
            fn GetCurrentThreadId() -> u32;
        }
        unsafe { GetThreadId(thread.as_raw_handle()) == GetCurrentThreadId() }
    }
}

/// The delivery thread: begins each waiting ComPrimitive once the one before it has finished,
/// queues each of its events when it is due and notifies the callback, with the state unlocked,
/// as `queue_event` does.
fn deliver() {
    let mut guard = state().lock().expect("mock state poisoned");
    loop {
        if guard.stop_delivery {
            return;
        }
        let now = Instant::now();
        match guard.scheduled.front().map(|(due, _)| *due) {
            None if !guard.waiting.is_empty() => begin_next(&mut guard, now),
            None => {
                guard = delivery_signal().wait(guard).expect("mock state poisoned");
            }
            Some(due) if due > now => {
                guard = delivery_signal()
                    .wait_timeout(guard, due - now)
                    .expect("mock state poisoned")
                    .0;
            }
            Some(_) => {
                let (_, event) = guard.scheduled.pop_front().expect("an event is due");
                guard.pending_events.push(event.clone());
                let (callback, api_tag) = (guard.callback, guard.api_tag);
                drop(guard);
                unsafe { notify_data_available(callback, &event, api_tag) };
                guard = state().lock().expect("mock state poisoned");
            }
        }
    }
}

/// How a send-and-receive ComPrimitive's control data says to run it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cycles {
    /// Nothing to send: no request reaches the ECU.
    NoSend,
    /// Send without waiting for a response (`NumReceiveCycles` 0).
    SendOnly,
    /// Send and receive one response. Periodic sends and several or cyclic receives are run as
    /// one send and one response.
    SendReceive,
}

/// `PDU_COP_CTRL_DATA`'s send and receive cycles (ISO 22900-2 (2022) 8.2.6.2). Without control
/// data the mock sends once and receives once.
fn cycles(control: Option<&PDU_COP_CTRL_DATA>) -> Cycles {
    match control {
        None => Cycles::SendReceive,
        Some(control) if control.NumSendCycles == 0 => Cycles::NoSend,
        Some(control) if control.NumReceiveCycles == 0 => Cycles::SendOnly,
        Some(_) => Cycles::SendReceive,
    }
}

/// The events a `PDU_COPT_SENDRECV` ComPrimitive gets from the attached ECU, each with its delay
/// from the request in milliseconds (ADR-299): the `PDU_COPST_EXECUTING` status item; when it
/// receives, a result item for each response pending message and for the response, or a receive
/// timeout error item when the ECU sends no response; then the `PDU_COPST_FINISHED` status item.
fn ecu_events(
    ecu: &mut SimEcu,
    request: &[u8],
    cycles: Cycles,
    event: &PendingEvent,
) -> Vec<(u32, PendingEvent)> {
    let with = |kind| PendingEvent {
        kind,
        ..event.clone()
    };
    let mut events = vec![(
        0,
        with(EventKind::Status(E_PDU_STATUS::PDU_COPST_EXECUTING)),
    )];
    if cycles == Cycles::NoSend {
        events.push((0, with(EventKind::Status(E_PDU_STATUS::PDU_COPST_FINISHED))));
        return events;
    }
    let exchange = ecu.exchange(Addressing::Physical, request);
    if cycles == Cycles::SendOnly {
        // The request reached the ECU; whatever it answers is not received.
        events.push((0, with(EventKind::Status(E_PDU_STATUS::PDU_COPST_FINISHED))));
        return events;
    }
    events.extend(
        exchange
            .pending
            .into_iter()
            .map(|(delay_ms, bytes)| (delay_ms, with(EventKind::Result(bytes)))),
    );
    let last = match exchange.response.to_bytes() {
        Some(bytes) => {
            events.push((exchange.delay_ms, with(EventKind::Result(bytes))));
            exchange.delay_ms
        }
        None => {
            // The mock keeps no ComParams, so there is no P2 to wait for: the timeout is
            // reported right after the last message the ECU sent.
            let at = events.last().map_or(0, |(delay_ms, _)| *delay_ms);
            events.push((
                at,
                with(EventKind::Error(
                    E_PDU_ERR_EVT::PDU_ERR_EVT_RX_TIMEOUT,
                    MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP,
                )),
            ));
            at
        }
    };
    events.push((
        last,
        with(EventKind::Status(E_PDU_STATUS::PDU_COPST_FINISHED)),
    ));
    events
}

/// The `EcuConfig` in the file `path` names, if it names one (`NGR_SIM_ECU_CONFIG`); `Err` when
/// the file cannot be read or parsed, or holds a value `EcuConfig::validate` refuses.
fn ecu_config_from(path: Option<std::ffi::OsString>) -> Result<Option<EcuConfig>, ()> {
    match path {
        None => Ok(None),
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|_| ())?;
            let config: EcuConfig = serde_json::from_str(&text).map_err(|_| ())?;
            config.validate().map_err(|_| ())?;
            Ok(Some(config))
        }
    }
}

/// The configuration an ECU attached without one gets, as `sim-vci`'s default.
fn default_ecu_config() -> EcuConfig {
    EcuConfig {
        vin: "NGRSIMECU00000001".into(),
        part_number: "NGR-SIM-ECU".into(),
        sw_version: "1.0.0".into(),
        ..Default::default()
    }
}

/// A command for `__mock_sim_ecu_control`, as JSON.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SimEcuCommand {
    /// Attaches a fresh simulated ECU, replacing any attached one. Its configuration is
    /// `sim-vci`'s default with the fields `config` names (an `EcuConfig` JSON object, whole
    /// or in part) set as given.
    Attach {
        #[serde(default)]
        config: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// Detaches the ECU: ComPrimitives get the synthetic completion event again.
    Detach {},
    /// Arms a `sim_ecu::Fault` on the attached ECU (`SimEcu::inject`), or, with `service`, once
    /// a request of that service reaches it, or, with `request`, once a request that starts
    /// with those bytes does. At most one of `service` and `request`.
    InjectFault {
        fault: Fault,
        #[serde(default)]
        service: Option<u8>,
        #[serde(default)]
        request: Option<Vec<u8>>,
    },
}

fn apply_sim_ecu_command(state: &mut MockState, command: SimEcuCommand) -> Result<(), ()> {
    match command {
        SimEcuCommand::Attach { config } => {
            let mut merged = serde_json::to_value(default_ecu_config()).map_err(|_| ())?;
            let fields = merged.as_object_mut().ok_or(())?;
            let config = config.unwrap_or_default();
            // A misspelled field would otherwise leave the default in place unnoticed.
            if config.keys().any(|key| !fields.contains_key(key)) {
                return Err(());
            }
            fields.extend(config);
            let config: EcuConfig = serde_json::from_value(merged).map_err(|_| ())?;
            config.validate().map_err(|_| ())?;
            state.ecu = Some(SimEcu::new(config));
        }
        SimEcuCommand::Detach {} => state.ecu = None,
        SimEcuCommand::InjectFault {
            fault,
            service,
            request,
        } => {
            let ecu = state.ecu.as_mut().ok_or(())?;
            match (service, request) {
                (None, None) => ecu.inject(fault),
                (Some(sid), None) => ecu.inject_for_service(sid, fault),
                (None, Some(prefix)) => ecu.inject_for_request(&prefix, fault),
                (Some(_), Some(_)) => return Err(()),
            }
        }
    }
    Ok(())
}

#[cfg(all(windows, target_arch = "x86"))]
macro_rules! exported_fn {
    ($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty $body:block) => {
        /// # Safety
        ///
        /// This function is exported as part of the mock D-PDU API's C ABI.
        /// The caller must uphold the same contract as the real D-PDU API
        /// entry point of this name: any pointer argument must be either
        /// null or point to a valid, correctly-sized, and appropriately
        /// aligned instance of the expected type for the duration of the
        /// call, and the caller must not assume thread-safety beyond what
        /// the D-PDU API specifies.
        #[unsafe(no_mangle)]
        pub unsafe extern "stdcall" fn $name($($arg: $ty),*) -> $ret $body
    };
}

#[cfg(not(all(windows, target_arch = "x86")))]
macro_rules! exported_fn {
    ($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty $body:block) => {
        /// # Safety
        ///
        /// This function is exported as part of the mock D-PDU API's C ABI.
        /// The caller must uphold the same contract as the real D-PDU API
        /// entry point of this name: any pointer argument must be either
        /// null or point to a valid, correctly-sized, and appropriately
        /// aligned instance of the expected type for the duration of the
        /// call, and the caller must not assume thread-safety beyond what
        /// the D-PDU API specifies.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret $body
    };
}

exported_fn!(PDUConstruct(_option_str: *mut CHAR8, p_api_tag: *mut c_void) -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    if guard.ecu.is_none() {
        match ecu_config_from(std::env::var_os(ECU_CONFIG_ENV)) {
            Ok(Some(config)) => guard.ecu = Some(SimEcu::new(config)),
            Ok(None) => {}
            Err(()) => return E_PDU_ERROR::PDU_ERR_FCT_FAILED,
        }
    }
    // A delivery thread that a `PDUDestruct` from its own callback left to end keeps the stop
    // flag set until here.
    let mut thread = delivery_thread().lock().expect("delivery thread poisoned");
    if thread.as_ref().is_some_and(JoinHandle::is_finished) {
        let _ = thread.take().map(JoinHandle::join);
        guard.stop_delivery = false;
    }
    drop(thread);
    guard.constructed = true;
    guard.api_tag = p_api_tag as usize;
    guard.next_com_primitive_handle = MOCK_COM_PRIMITIVE_HANDLE;
    guard.pending_events.clear();
    guard.waiting.clear();
    guard.scheduled.clear();
    guard.counters.construct += 1;
    no_error()
});

exported_fn!(PDUDestruct() -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.constructed = false;
    guard.api_tag = 0;
    guard.callback = None;
    guard.pending_events.clear();
    guard.counters.destruct += 1;
    // The caller frees the API tag and may unload the library once this returns, so no
    // delivery may still be running.
    stop_delivery_thread(guard);
    no_error()
});

exported_fn!(PDUIoCtl(_h_mod: UNUM32, _h_cll: UNUM32, io_ctl_command_id: T_PDU_IT, p_input_data: *mut PDU_DATA_ITEM, p_output_data: *mut *mut PDU_DATA_ITEM) -> T_PDU_ERROR {
    if io_ctl_command_id.0 as UNUM32 == MOCK_IOCTL_NULL_OUTPUT {
        return no_error();
    }
    if !p_output_data.is_null() {
        let input_type = if p_input_data.is_null() {
            io_ctl_command_id
        } else {
            unsafe { (*p_input_data).ItemType }
        };

        let output_ptr = match input_type {
            E_PDU_IT::PDU_IT_IO_PROG_VOLTAGE => {
                let payload = Box::new(PDU_IO_PROG_VOLTAGE_DATA {
                    ProgVoltage_mv: 12000,
                    PinOnDLC: 1,
                });
                let payload_ptr = Box::into_raw(payload).cast::<c_void>();
                Box::into_raw(Box::new(PDU_DATA_ITEM {
                    ItemType: E_PDU_IT::PDU_IT_IO_PROG_VOLTAGE,
                    pData: payload_ptr,
                }))
            }
            E_PDU_IT::PDU_IT_IO_EVENT_QUEUE_PROPERTY => {
                let payload = Box::new(PDU_IO_EVENT_QUEUE_PROPERTY_DATA {
                    QueueSize: 4,
                    QueueMode: E_PDU_QUEUE_MODE::PDU_QUE_CIRCULAR,
                });
                let payload_ptr = Box::into_raw(payload).cast::<c_void>();
                Box::into_raw(Box::new(PDU_DATA_ITEM {
                    ItemType: E_PDU_IT::PDU_IT_IO_EVENT_QUEUE_PROPERTY,
                    pData: payload_ptr,
                }))
            }
            E_PDU_IT::PDU_IT_IO_VEHICLE_ID_REQUEST => {
                let payload = Box::new(PDU_IO_VEHICLE_ID_REQUEST {
                    PreselectionMode: 1,
                    PreselectionValue: MOCK_VEHICLE_NAME.as_ptr() as *mut CHAR8,
                    CombinationMode: 0,
                    VehicleDiscoveryTime: 100,
                    NumDestinationAddresses: 0,
                    pDestinationAddresses: ptr::null_mut(),
                });
                let payload_ptr = Box::into_raw(payload).cast::<c_void>();
                Box::into_raw(Box::new(PDU_DATA_ITEM {
                    ItemType: E_PDU_IT::PDU_IT_IO_VEHICLE_ID_REQUEST,
                    pData: payload_ptr,
                }))
            }
            _ => {
                let payload = Box::new(PDU_IO_BYTEARRAY_DATA {
                    DataSize: 0,
                    pData: ptr::null_mut(),
                });
                let payload_ptr = Box::into_raw(payload).cast::<c_void>();
                Box::into_raw(Box::new(PDU_DATA_ITEM {
                    ItemType: input_type,
                    pData: payload_ptr,
                }))
            }
        };

        unsafe {
            *p_output_data = output_ptr;
        }
    }

    no_error()
});

exported_fn!(PDUGetVersion(_h_mod: UNUM32, p_version_data: *mut PDU_VERSION_DATA) -> T_PDU_ERROR {
    {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.counters.get_version += 1;
    }
    unsafe { set_out(p_version_data, fixed_version()) }
});

exported_fn!(PDUGetStatus(_h_mod: UNUM32, _h_cll: UNUM32, h_cop: UNUM32, p_status_code: *mut T_PDU_STATUS, p_timestamp: *mut UNUM32, p_extra_info: *mut UNUM32) -> T_PDU_ERROR {
    if p_status_code.is_null() || p_timestamp.is_null() || p_extra_info.is_null() {
        return invalid_parameters();
    }

    let status = {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.counters.get_status += 1;
        // A ComPrimitive waiting for its turn is idle, one with events to come is executing
        // (ADR-299).
        if h_cop != 0 && guard.waiting.iter().any(|cop| cop.event.h_cop == h_cop) {
            E_PDU_STATUS::PDU_COPST_IDLE
        } else if h_cop != 0 && guard.scheduled.iter().any(|(_, event)| event.h_cop == h_cop) {
            E_PDU_STATUS::PDU_COPST_EXECUTING
        } else {
            E_PDU_STATUS::PDU_COPST_FINISHED
        }
    };

    unsafe {
        *p_status_code = status;
        *p_timestamp = MOCK_TIMESTAMP;
        *p_extra_info = 0;
    }
    no_error()
});

exported_fn!(PDUGetLastError(_h_mod: UNUM32, _h_cll: UNUM32, p_error_code: *mut T_PDU_ERR_EVT, ph_cop: *mut UNUM32, p_timestamp: *mut UNUM32, p_extra_error_info: *mut UNUM32) -> T_PDU_ERROR {
    if p_error_code.is_null() || ph_cop.is_null() || p_timestamp.is_null() || p_extra_error_info.is_null() {
        return invalid_parameters();
    }

    let report = {
        let guard = state().lock().expect("mock state poisoned");
        guard.last_error_report
    };

    unsafe {
        *p_error_code = report.error_code;
        *ph_cop = report.cop_handle;
        *p_timestamp = report.timestamp;
        *p_extra_error_info = report.extra_error_info;
    }
    no_error()
});

exported_fn!(PDUGetResourceStatus(p_resource_status: *mut PDU_RSC_STATUS_ITEM) -> T_PDU_ERROR {
    if p_resource_status.is_null() {
        return invalid_parameters();
    }

    let item = unsafe { &mut *p_resource_status };
    if item.NumEntries == 0 || item.pResourceStatusData.is_null() {
        return invalid_parameters();
    }

    let entries = unsafe { std::slice::from_raw_parts_mut(item.pResourceStatusData, item.NumEntries as usize) };
    for entry in entries {
        entry.ResourceStatus = 0;
    }
    no_error()
});

exported_fn!(PDUCreateComLogicalLink(_h_mod: UNUM32, _p_rsc_data: *mut PDU_RSC_DATA, _resource_id: UNUM32, _p_cll_tag: *mut c_void, ph_cll: *mut UNUM32, _p_cll_create_flag: *mut PDU_FLAG_DATA) -> T_PDU_ERROR {
    if ph_cll.is_null() {
        return invalid_parameters();
    }

    {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.counters.create_com_logical_link += 1;
    }

    unsafe {
        *ph_cll = MOCK_LOGICAL_LINK_HANDLE;
    }
    no_error()
});

exported_fn!(PDUDestroyComLogicalLink(_h_mod: UNUM32, _h_cll: UNUM32) -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.destroy_com_logical_link += 1;
    drop(guard);
    no_error()
});

exported_fn!(PDUConnect(_h_mod: UNUM32, _h_cll: UNUM32) -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.connect += 1;
    drop(guard);
    no_error()
});

exported_fn!(PDUDisconnect(_h_mod: UNUM32, _h_cll: UNUM32) -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.counters.disconnect += 1;
    drop(guard);
    no_error()
});

exported_fn!(PDULockResource(_h_mod: UNUM32, _h_cll: UNUM32, _lock_mask: UNUM32) -> T_PDU_ERROR {
    // Error injection (`__mock_set_lock_resource_error`): checked before the
    // (currently unconditional) success behavior, mirroring
    // `j2534-0404-mock`'s `PassThruSetProgrammingVoltage` override check.
    let guard = state().lock().expect("mock state poisoned");
    if let Some(code) = guard.lock_resource_error {
        return code;
    }
    drop(guard);
    no_error()
});

exported_fn!(PDUUnlockResource(_h_mod: UNUM32, _h_cll: UNUM32, _lock_mask: UNUM32) -> T_PDU_ERROR {
    no_error()
});

exported_fn!(PDUGetComParam(_h_mod: UNUM32, _h_cll: UNUM32, param_id: UNUM32, p_param_item: *mut *mut PDU_PARAM_ITEM) -> T_PDU_ERROR {
    unsafe { set_out_ptr(p_param_item, make_param_item(param_id)) }
});

exported_fn!(PDUSetComParam(_h_mod: UNUM32, _h_cll: UNUM32, _p_param_item: *mut PDU_PARAM_ITEM) -> T_PDU_ERROR {
    no_error()
});

exported_fn!(PDUStartComPrimitive(_h_mod: UNUM32, _h_cll: UNUM32, cop_type: T_PDU_COPT, cop_data_size: UNUM32, p_cop_data: *mut UNUM8, p_cop_ctrl_data: *mut PDU_COP_CTRL_DATA, p_cop_tag: *mut c_void, ph_cop: *mut UNUM32) -> T_PDU_ERROR {
    if ph_cop.is_null() {
        return invalid_parameters();
    }
    let request: &[u8] = if p_cop_data.is_null() || cop_data_size == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(p_cop_data, cop_data_size as usize) }
    };

    let control = unsafe { p_cop_ctrl_data.as_ref() };

    let (handle, immediate) = {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.counters.start_com_primitive += 1;
        let handle = guard.next_primitive_handle();
        let event = PendingEvent {
            h_mod: MOCK_MODULE_HANDLE,
            h_cll: MOCK_LOGICAL_LINK_HANDLE,
            h_cop: handle,
            tag: p_cop_tag as usize,
            timestamp: MOCK_TIMESTAMP,
            kind: EventKind::Result(Vec::new()),
        };
        // While an ECU is attached, and while other ComPrimitives still wait or run, the
        // ComPrimitive waits its turn, and the ECU answers a send-and-receive request when it
        // begins (ADR-299). Otherwise the synthetic completion event is queued at once.
        let busy = !guard.waiting.is_empty() || !guard.scheduled.is_empty();
        let immediate = if guard.ecu.is_some() || busy {
            let request = (cop_type == E_PDU_COPT::PDU_COPT_SENDRECV && !request.is_empty())
                .then(|| (request.to_vec(), cycles(control)));
            guard.waiting.push_back(WaitingCop { event, request });
            // On an idle link it begins at once, as it would without the delivery thread.
            if !busy {
                begin_next(&mut guard, Instant::now());
            }
            delivery_signal().notify_all();
            // Started under the state lock, so a `PDUDestruct` that follows finds the thread
            // to stop; none is started once the API is destructed.
            if guard.constructed {
                ensure_delivery_thread();
            }
            None
        } else {
            Some(event)
        };
        (handle, immediate)
    };

    unsafe {
        *ph_cop = handle;
    }

    if let Some(event) = immediate {
        unsafe { queue_event(event) };
    }
    no_error()
});

exported_fn!(PDUCancelComPrimitive(_h_mod: UNUM32, _h_cll: UNUM32, _h_cop: UNUM32) -> T_PDU_ERROR {
    no_error()
});

exported_fn!(PDUGetEventItem(_h_mod: UNUM32, _h_cll: UNUM32, p_event_item: *mut *mut PDU_EVENT_ITEM) -> T_PDU_ERROR {
    let event = {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.counters.get_event_item += 1;
        if guard.pending_events.is_empty() {
            None
        } else {
            Some(guard.pending_events.remove(0))
        }
    };

    match event {
        Some(event) => unsafe { set_out_ptr(p_event_item, make_event_item(event)) },
        None => event_queue_empty(),
    }
});

exported_fn!(PDUDestroyItem(p_item: *mut PDU_ITEM) -> T_PDU_ERROR {
    if p_item.is_null() {
        return invalid_parameters();
    }

    let item_type = unsafe { (*p_item).ItemType };
    match item_type {
        E_PDU_IT::PDU_IT_MODULE_ID => {
            let item = p_item.cast::<PDU_MODULE_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pModuleData.is_null() {
                    let _ = Vec::from_raw_parts(
                        item.pModuleData,
                        item.NumEntries as usize,
                        item.NumEntries as usize,
                    );
                }
            }
        }
        E_PDU_IT::PDU_IT_RSC_STATUS => {
            let item = p_item.cast::<PDU_RSC_STATUS_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pResourceStatusData.is_null() {
                    let _ = Vec::from_raw_parts(
                        item.pResourceStatusData,
                        item.NumEntries as usize,
                        item.NumEntries as usize,
                    );
                }
            }
        }
        E_PDU_IT::PDU_IT_RSC_ID => {
            let item = p_item.cast::<PDU_RSC_ID_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pResourceIdDataArray.is_null() {
                    let entries = Vec::from_raw_parts(
                        item.pResourceIdDataArray,
                        item.NumModules as usize,
                        item.NumModules as usize,
                    );
                    for entry in entries {
                        if !entry.pResourceIdArray.is_null() {
                            let _ = Vec::from_raw_parts(
                                entry.pResourceIdArray,
                                entry.NumIds as usize,
                                entry.NumIds as usize,
                            );
                        }
                    }
                }
            }
        }
        E_PDU_IT::PDU_IT_RSC_CONFLICT => {
            let item = p_item.cast::<PDU_RSC_CONFLICT_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pRscConflictData.is_null() {
                    let _ = Vec::from_raw_parts(
                        item.pRscConflictData,
                        item.NumEntries as usize,
                        item.NumEntries as usize,
                    );
                }
            }
        }
        E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE => {
            let item = p_item.cast::<PDU_UNIQUE_RESP_ID_TABLE_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pUniqueData.is_null() {
                    let entries = Vec::from_raw_parts(
                        item.pUniqueData,
                        item.NumEntries as usize,
                        item.NumEntries as usize,
                    );
                    for entry in entries {
                        if !entry.pParams.is_null() {
                            let _ = Vec::from_raw_parts(
                                entry.pParams,
                                entry.NumParamItems as usize,
                                entry.NumParamItems as usize,
                            );
                        }
                    }
                }
            }
        }
        E_PDU_IT::PDU_IT_RESULT => {
            let item = p_item.cast::<PDU_EVENT_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pData.is_null() {
                    let data = Box::from_raw(item.pData.cast::<PDU_RESULT_DATA>());
                    if !data.pDataBytes.is_null() {
                        let _ = Box::from_raw(ptr::slice_from_raw_parts_mut(
                            data.pDataBytes,
                            data.NumDataBytes as usize,
                        ));
                    }
                }
            }
        }
        E_PDU_IT::PDU_IT_STATUS => {
            let item = p_item.cast::<PDU_EVENT_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pData.is_null() {
                    let _ = Box::from_raw(item.pData.cast::<T_PDU_STATUS>());
                }
            }
        }
        E_PDU_IT::PDU_IT_ERROR => {
            let item = p_item.cast::<PDU_EVENT_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pData.is_null() {
                    let _ = Box::from_raw(item.pData.cast::<PDU_ERROR_DATA>());
                }
            }
        }
        E_PDU_IT::PDU_IT_INFO => {
            let item = p_item.cast::<PDU_EVENT_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pData.is_null() {
                    let _ = Box::from_raw(item.pData.cast::<PDU_RESULT_DATA>());
                }
            }
        }
        E_PDU_IT::PDU_IT_PARAM => {
            let item = p_item.cast::<PDU_PARAM_ITEM>();
            unsafe {
                let item = Box::from_raw(item);
                if !item.pComParamData.is_null() {
                    let _ = Box::from_raw(item.pComParamData.cast::<u32>());
                }
            }
        }
        _ => {
            unsafe {
                let _ = Box::from_raw(p_item.cast::<PDU_ITEM>());
            }
        }
    }

    no_error()
});

exported_fn!(PDURegisterEventCallback(_h_mod: UNUM32, _h_cll: UNUM32, event_callback_function: CALLBACKFNC) -> T_PDU_ERROR {
    let (pending_events, api_tag) = {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.callback = event_callback_function;
        (guard.pending_events.clone(), guard.api_tag)
    };

    // Invoke callback immediately for any already-pending events
    for event in pending_events {
        unsafe { notify_data_available(event_callback_function, &event, api_tag) };
    }

    no_error()
});

exported_fn!(PDUGetObjectId(pdu_object_type: T_PDU_OBJT, p_shortname: *mut CHAR8, p_pdu_object_id: *mut UNUM32) -> T_PDU_ERROR {
    if p_shortname.is_null() || p_pdu_object_id.is_null() {
        return invalid_parameters();
    }

    let short_name = unsafe { std::ffi::CStr::from_ptr(p_shortname.cast()) }
        .to_str()
        .unwrap_or_default();
    let object_id = match (pdu_object_type, short_name) {
        (E_PDU_OBJT::PDU_OBJT_BUSTYPE, "mock-bus") => 3101,
        (E_PDU_OBJT::PDU_OBJT_PROTOCOL, "mock-protocol") => 3102,
        (E_PDU_OBJT::PDU_OBJT_RESOURCE, "mock-resource") => MOCK_RESOURCE_ID,
        (E_PDU_OBJT::PDU_OBJT_COMPARAM, "mock-com-param") => 3201,
        (E_PDU_OBJT::PDU_OBJT_PINTYPE, "mock-pin-type") => 3301,
        // The ISO 22900-2 (2022) short names of UDS on CAN (Annex B.1, B.2), which the agent
        // names a link's resource by (ADR-319).
        (E_PDU_OBJT::PDU_OBJT_BUSTYPE, "ISO_11898_2_DWCAN") => STANDARD_BUS_TYPE_DWCAN,
        (E_PDU_OBJT::PDU_OBJT_PROTOCOL, "ISO_14229_3_on_ISO_15765_2") => {
            STANDARD_PROTOCOL_UDS_ON_CAN
        }
        (E_PDU_OBJT::PDU_OBJT_PINTYPE, "HI") => STANDARD_PIN_TYPE_HI,
        (E_PDU_OBJT::PDU_OBJT_PINTYPE, "LOW") => STANDARD_PIN_TYPE_LOW,
        (E_PDU_OBJT::PDU_OBJT_COMPARAM, name) => match standard_comparam_id(name) {
            Some(id) => id,
            None => return invalid_parameters(),
        },
        // An unrecognized (object type, shortname) pair is a client error,
        // matching how a real D-PDU DLL's PDUGetObjectId rejects an unknown
        // shortname (PDU_ERR_INVALID_PARAMETERS) rather than silently
        // defaulting to id 0.
        _ => return invalid_parameters(),
    };

    unsafe {
        *p_pdu_object_id = object_id;
    }
    no_error()
});

exported_fn!(PDUGetModuleIds(p_module_id_list: *mut *mut PDU_MODULE_ITEM) -> T_PDU_ERROR {
    unsafe { set_out_ptr(p_module_id_list, make_module_item()) }
});

exported_fn!(PDUGetResourceIds(h_mod: UNUM32, _p_resource_id_data: *mut PDU_RSC_DATA, p_resource_id_list: *mut *mut PDU_RSC_ID_ITEM) -> T_PDU_ERROR {
    unsafe { set_out_ptr(p_resource_id_list, make_resource_id_item(h_mod)) }
});

exported_fn!(PDUGetConflictingResources(_resource_id: UNUM32, _p_input_module_list: *mut PDU_MODULE_ITEM, p_output_conflict_list: *mut *mut PDU_RSC_CONFLICT_ITEM) -> T_PDU_ERROR {
    unsafe { set_out_ptr(p_output_conflict_list, make_conflict_item()) }
});

exported_fn!(PDUGetUniqueRespIdTable(_h_mod: UNUM32, _h_cll: UNUM32, p_unique_resp_id_table: *mut *mut PDU_UNIQUE_RESP_ID_TABLE_ITEM) -> T_PDU_ERROR {
    unsafe { set_out_ptr(p_unique_resp_id_table, make_unique_resp_table_item()) }
});

exported_fn!(PDUSetUniqueRespIdTable(_h_mod: UNUM32, _h_cll: UNUM32, _p_unique_resp_id_table: *mut PDU_UNIQUE_RESP_ID_TABLE_ITEM) -> T_PDU_ERROR {
    no_error()
});

exported_fn!(PDUModuleConnect(_h_mod: UNUM32) -> T_PDU_ERROR {
    no_error()
});

exported_fn!(PDUModuleDisconnect(_h_mod: UNUM32) -> T_PDU_ERROR {
    no_error()
});

exported_fn!(PDUGetTimestamp(_h_mod: UNUM32, p_timestamp: *mut UNUM32) -> T_PDU_ERROR {
    unsafe { set_out(p_timestamp, MOCK_TIMESTAMP) }
});

// Exported FFI back-door: Reset all mock state for test isolation.
// Call this from tests to reset the DLL's internal state between test runs.
exported_fn!(__mock_reset() -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    // A `PDUDestruct` in progress still needs the delivery thread to see its stop.
    let stop_delivery = guard.stop_delivery;
    *guard = MockState::default();
    guard.stop_delivery = stop_delivery;
    no_error()
});

// Exported FFI back-door: controls the simulated ECU (ADR-299). `command` is a NUL-terminated
// JSON `SimEcuCommand`; returns 0 when it applied, 1 when it did not parse or could not apply
// (a fault with no ECU attached, both `service` and `request`, an `attach` with a field `EcuConfig` lacks, or one with a value `EcuConfig::validate` refuses).
exported_fn!(__mock_sim_ecu_control(command: *const c_char) -> UNUM32 {
    if command.is_null() {
        return 1;
    }
    let Ok(text) = unsafe { CStr::from_ptr(command) }.to_str() else {
        return 1;
    };
    let Ok(command) = serde_json::from_str::<SimEcuCommand>(text) else {
        return 1;
    };
    let mut guard = state().lock().expect("mock state poisoned");
    match apply_sim_ecu_command(&mut guard, command) {
        Ok(()) => 0,
        Err(()) => 1,
    }
});

// Exported FFI back-door: Get construct call count.
exported_fn!(__mock_get_construct_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.construct
});

// Exported FFI back-door: Get destroy call count.
exported_fn!(__mock_get_destruct_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.destruct
});

// Exported FFI back-door: Get version call count.
exported_fn!(__mock_get_version_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.get_version
});

// Exported FFI back-door: Get status call count.
exported_fn!(__mock_get_status_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.get_status
});

// Exported FFI back-door: Get create_com_logical_link call count.
exported_fn!(__mock_get_create_com_logical_link_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.create_com_logical_link
});

// Exported FFI back-door: Get destroy_com_logical_link call count.
exported_fn!(__mock_get_destroy_com_logical_link_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.destroy_com_logical_link
});

// Exported FFI back-door: Get connect call count.
exported_fn!(__mock_get_connect_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.connect
});

// Exported FFI back-door: Get disconnect call count.
exported_fn!(__mock_get_disconnect_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.disconnect
});

// Exported FFI back-door: Get start_com_primitive call count.
exported_fn!(__mock_get_start_com_primitive_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.start_com_primitive
});

// Exported FFI back-door: Get event_item call count.
exported_fn!(__mock_get_event_item_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.get_event_item
});

// Exported FFI back-door: Force PDULockResource to fail with the given
// T_PDU_ERROR on every subsequent call instead of its normal success
// behavior -- lets tests exercise iso22900-service's
// `with_api_for_link`/`map_runtime_error_for_link` CLL-scoped
// native-failure path (ADR-105 P2 backlog) without a real adapter.
// `code == PDU_STATUS_NOERROR` (`0`) clears the override (the default after
// `__mock_reset`), mirroring `j2534-0404-mock`'s
// `__mock_set_prog_voltage_error` convention.
exported_fn!(__mock_set_lock_resource_error(code: T_PDU_ERROR) -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.lock_resource_error = if code == no_error() { None } else { Some(code) };
    no_error()
});

// Exported FFI back-door: Configure the response PDUGetLastError reports on
// every subsequent call -- lets tests drive the best-effort last-error fetch
// in iso22900-service's `with_api_for_link`/`map_runtime_error_for_link`
// (ADR-105 P2 backlog), including the `PDU_ERR_EVT_NOERROR`-filtering case.
// Reset to `PDU_ERR_EVT_NOERROR`/0/0/0 (this mock's always-no-error behavior
// prior to this back-door) by `__mock_reset`.
exported_fn!(__mock_set_last_error(error_code: T_PDU_ERR_EVT, cop_handle: UNUM32, timestamp: UNUM32, extra_error_info: UNUM32) -> T_PDU_ERROR {
    let mut guard = state().lock().expect("mock state poisoned");
    guard.last_error_report = LastErrorReport {
        error_code,
        cop_handle,
        timestamp,
        extra_error_info,
    };
    no_error()
});

pub fn mock_library_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "iso22900_mock.dll"
    } else if cfg!(target_os = "macos") {
        "libiso22900_mock.dylib"
    } else {
        "libiso22900_mock.so"
    }
}

fn is_mock_library_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case(mock_library_file_name()))
        .unwrap_or(false)
}

/// Recursively collects every mock library file under `dir` into `found`.
fn collect_mock_libraries(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_mock_library_file(&path) {
            found.push(path);
        } else if path.is_dir() {
            collect_mock_libraries(&path, found);
        }
    }
}

/// Returns the most recently modified mock library under `dir`. The target
/// directory can hold several copies (e.g. Cargo's uplifted
/// `target/debug/lib*.so` next to the freshly compiled artifact in
/// `target/debug/deps/`), and the uplifted copy is not refreshed by every
/// build — picking the first directory-walk hit could load a stale build
/// whose FFI constants disagree with the code under test.
fn search_directory(dir: &Path) -> Option<PathBuf> {
    let mut found = Vec::new();
    collect_mock_libraries(dir, &mut found);
    found
        .into_iter()
        .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

/// The `file:` URI of an absolute `path`, as a root description file's `LIBRARY_FILE` names it:
/// `file:///opt/x.so` on Unix, `file:///C:/x.dll` on Windows. Bytes outside the unreserved set
/// (and `/`, `:`) are percent-encoded, so a path with spaces or non-ASCII characters round-trips.
///
/// # Panics
///
/// If `path` is not absolute.
pub fn file_uri(path: &Path) -> String {
    assert!(
        path.is_absolute(),
        "not an absolute path: {}",
        path.display()
    );
    let text = path.to_string_lossy();
    // A verbatim drive path (`\\?\C:\x`) names the same file as `C:\x`.
    #[cfg(windows)]
    let text = text
        .strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .replace('\\', "/");
    let mut uri = String::from(if text.starts_with('/') {
        "file://"
    } else {
        "file:///"
    });
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// Writes a D-PDU API root description file `pdu_api_root.xml` into `dir` with one entry,
/// `short_name`, whose `LIBRARY_FILE` names `library`, and returns its path. Tests point the
/// debug-only `NGR_PDU_API_ROOT_FILE` override at it (ADR-282), or pass it to
/// `ngr_library_resolver::iso22900::resolve_in_root_file`, to have a service resolve the mock.
pub fn write_root_file(
    dir: &Path,
    short_name: &str,
    library: &Path,
) -> Result<PathBuf, std::io::Error> {
    write_root_file_entry(dir, short_name, library, None)
}

/// [`write_root_file`] with an entry that also names `module_description_file` as its MDF
/// (`MODULE_DESCRIPTION_FILE`, ISO 22900-2:2022 Annex F).
pub fn write_root_file_with_mdf(
    dir: &Path,
    short_name: &str,
    library: &Path,
    module_description_file: &Path,
) -> Result<PathBuf, std::io::Error> {
    write_root_file_entry(dir, short_name, library, Some(module_description_file))
}

fn write_root_file_entry(
    dir: &Path,
    short_name: &str,
    library: &Path,
    module_description_file: Option<&Path>,
) -> Result<PathBuf, std::io::Error> {
    let root_file = dir.join("pdu_api_root.xml");
    // The name is element text: escape the characters XML gives a meaning there. The URI needs
    // none, since `file_uri` percent-encodes `&`, `<` and `"`.
    let short_name = xml_text(short_name);
    let mdf = module_description_file
        .map(|mdf| format!("    <MODULE_DESCRIPTION_FILE URI=\"{}\"/>\n", file_uri(mdf)))
        .unwrap_or_default();
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <MVCI_PDU_API_ROOT>\n  <MVCI_PDU_API>\n    <SHORT_NAME>{short_name}</SHORT_NAME>\n    \
         <LIBRARY_FILE URI=\"{}\"/>\n{mdf}  </MVCI_PDU_API>\n</MVCI_PDU_API_ROOT>\n",
        file_uri(library)
    );
    std::fs::write(&root_file, xml)?;
    Ok(root_file)
}

/// The name the mock's MDF gives [`MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP`].
pub const MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP_NAME: &str = "MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP";

/// Writes a module description file `mock_mdf.xml` into `dir` whose `ERROR_CODE` elements name
/// `error_codes` (ISO 22900-2:2022 Annex F, F.4), and returns its path. The mock reads no MDF
/// itself: the file is for `iso22900-service`, which names the extra error information of the mock's
/// error events by it.
pub fn write_module_description_file(
    dir: &Path,
    error_codes: &[(UNUM32, &str)],
) -> Result<PathBuf, std::io::Error> {
    let path = dir.join("mock_mdf.xml");
    let codes: String = error_codes
        .iter()
        .map(|(id, name)| {
            format!(
                "  <ERROR_CODE>\n    <ID>{id}</ID>\n    <SHORT_NAME>{}</SHORT_NAME>\n    \
                 <DESCRIPTION>iso22900-mock error code</DESCRIPTION>\n  </ERROR_CODE>\n",
                xml_text(name)
            )
        })
        .collect();
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <MVCI_MODULE_DESCRIPTION MVCI_PART2_STANDARD_VERSION=\"2.2.2\" FILE_VERSION=\"1\">\n\
         {codes}</MVCI_MODULE_DESCRIPTION>\n"
    );
    std::fs::write(&path, xml)?;
    Ok(path)
}

/// `text` escaped for XML element text.
fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn mock_library_path() -> Result<PathBuf, std::io::Error> {
    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        if let Some(found) = search_directory(&target_dir) {
            return Ok(found);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "could not find {} under {}",
                mock_library_file_name(),
                target_dir.display()
            ),
        ));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // The workspace target directory is the nearest ancestor that has one.
    let target_dir = manifest_dir
        .ancestors()
        .map(|dir| dir.join("target"))
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| manifest_dir.join("target"));
    if let Some(found) = search_directory(&target_dir) {
        return Ok(found);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "could not find {} under {}",
            mock_library_file_name(),
            target_dir.display()
        ),
    ))
}

/// Back-door API: Reset all mock state and counters for clean test isolation.
pub fn mock_reset() {
    let mut guard = state().lock().expect("mock state poisoned");
    let stop_delivery = guard.stop_delivery;
    *guard = MockState::default();
    guard.stop_delivery = stop_delivery;
}

/// Back-door API: Get the call count for PDUConstruct.
pub fn mock_get_construct_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.construct
}

/// Back-door API: Get the call count for PDUDestruct.
pub fn mock_get_destruct_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.destruct
}

/// Back-door API: Get the call count for PDUGetVersion.
pub fn mock_get_version_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.get_version
}

/// Back-door API: Get the call count for PDUGetStatus.
pub fn mock_get_status_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.get_status
}

/// Back-door API: Get the call count for PDUCreateComLogicalLink.
pub fn mock_get_create_com_logical_link_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.create_com_logical_link
}

/// Back-door API: Get the call count for PDUDestroyComLogicalLink.
pub fn mock_get_destroy_com_logical_link_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.destroy_com_logical_link
}

/// Back-door API: Get the call count for PDUConnect.
pub fn mock_get_connect_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.connect
}

/// Back-door API: Get the call count for PDUDisconnect.
pub fn mock_get_disconnect_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.disconnect
}

/// Back-door API: Get the call count for PDUStartComPrimitive.
pub fn mock_get_start_com_primitive_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.start_com_primitive
}

/// Back-door API: Get the call count for PDUGetEventItem.
pub fn mock_get_event_item_count() -> usize {
    let guard = state().lock().expect("mock state poisoned");
    guard.counters.get_event_item
}

#[cfg(test)]
mod library_path_tests {
    use super::*;

    /// `search_directory` must prefer the most recently modified copy: a
    /// stale uplifted `target/debug/lib*.so` must never shadow the freshly
    /// compiled artifact in `deps/` (or vice versa), or tests dynamically
    /// load a mock whose FFI constants disagree with the code under test.
    #[test]
    fn search_directory_prefers_the_newest_copy() {
        fn set_mtime(path: &Path, mtime: std::time::SystemTime) {
            let file = std::fs::File::options().append(true).open(path).unwrap();
            file.set_modified(mtime).unwrap();
        }

        let root = std::env::temp_dir().join(format!(
            "iso22900-mock-search-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let deps = root.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let uplifted = root.join(mock_library_file_name());
        let fresh = deps.join(mock_library_file_name());
        std::fs::write(&uplifted, b"stale").unwrap();
        std::fs::write(&fresh, b"fresh").unwrap();

        let base = std::time::SystemTime::now();
        set_mtime(&uplifted, base - std::time::Duration::from_secs(100));
        set_mtime(&fresh, base);
        assert_eq!(search_directory(&root), Some(fresh.clone()));

        // The preference is by mtime, not by location in the tree.
        set_mtime(&uplifted, base + std::time::Duration::from_secs(100));
        assert_eq!(search_directory(&root), Some(uplifted));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_root_file_escapes_the_short_name() {
        let dir = std::env::temp_dir().join(format!(
            "iso22900-mock-root-file-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let library = std::env::current_exe().unwrap();
        let root_file = write_root_file(&dir, "A&B <x>", &library).unwrap();
        let xml = std::fs::read_to_string(&root_file).unwrap();
        assert!(
            xml.contains("<SHORT_NAME>A&amp;B &lt;x&gt;</SHORT_NAME>"),
            "{xml}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn file_uri_percent_encodes_a_unix_path() {
        assert_eq!(
            file_uri(Path::new("/opt/a b/日.so")),
            "file:///opt/a%20b/%E6%97%A5.so"
        );
    }

    #[test]
    #[cfg(windows)]
    fn file_uri_uses_forward_slashes_on_windows() {
        assert_eq!(
            file_uri(Path::new(r"C:\a b\x.dll")),
            "file:///C:/a%20b/x.dll"
        );
    }
}

#[cfg(test)]
mod sim_ecu_tests {
    use super::*;

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "iso22900-mock-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn the_config_file_is_read_when_named() {
        assert!(matches!(ecu_config_from(None), Ok(None)));
        let config = EcuConfig {
            vin: "WVWZZZ1KZAW000001".into(),
            ..default_ecu_config()
        };
        let valid = temp_file("valid", &serde_json::to_string(&config).unwrap());
        let config = ecu_config_from(Some(valid.clone().into_os_string()))
            .expect("parses")
            .expect("a configuration");
        assert_eq!(config.vin, "WVWZZZ1KZAW000001");
        let invalid = temp_file("invalid", "not json");
        assert!(ecu_config_from(Some(invalid.clone().into_os_string())).is_err());
        let too_long = EcuConfig {
            part_number: "NGR-SIM-ECU-X".into(),
            ..default_ecu_config()
        };
        let too_long = temp_file("too-long", &serde_json::to_string(&too_long).unwrap());
        assert!(ecu_config_from(Some(too_long.clone().into_os_string())).is_err());
        std::fs::remove_file(too_long).unwrap();
        let missing = std::env::temp_dir().join("iso22900-mock-no-such-config.json");
        assert!(ecu_config_from(Some(missing.into_os_string())).is_err());
        std::fs::remove_file(valid).unwrap();
        std::fs::remove_file(invalid).unwrap();
    }

    #[test]
    fn faults_need_an_ecu_and_at_most_one_trigger() {
        let mut state = MockState::default();
        let command = |text: &str| serde_json::from_str::<SimEcuCommand>(text).unwrap();
        let fault = r#"{"inject_fault":{"fault":"drop_response"}}"#;
        assert!(apply_sim_ecu_command(&mut state, command(fault)).is_err());
        apply_sim_ecu_command(&mut state, command(r#"{"attach":{}}"#)).unwrap();
        apply_sim_ecu_command(&mut state, command(fault)).unwrap();
        let both = r#"{"inject_fault":{"fault":"drop_response","service":34,"request":[34]}}"#;
        assert!(apply_sim_ecu_command(&mut state, command(both)).is_err());
        apply_sim_ecu_command(&mut state, command(r#"{"detach":{}}"#)).unwrap();
        assert!(state.ecu.is_none());
        assert!(serde_json::from_str::<SimEcuCommand>(r#"{"reboot":{}}"#).is_err());
        let misspelled = r#"{"attach":{"config":{"s3_ms":300}}}"#;
        assert!(apply_sim_ecu_command(&mut state, command(misspelled)).is_err());
        assert!(state.ecu.is_none());
        let known = r#"{"attach":{"config":{"s3_server_ms":300}}}"#;
        apply_sim_ecu_command(&mut state, command(known)).unwrap();
        assert!(state.ecu.is_some());
        let too_long = r#"{"attach":{"config":{"sw_version":"1.0.0.1"}}}"#;
        state.ecu = None;
        assert!(apply_sim_ecu_command(&mut state, command(too_long)).is_err());
        assert!(state.ecu.is_none());
    }

    #[test]
    fn control_data_selects_the_cycles() {
        let control = |send, receive| PDU_COP_CTRL_DATA {
            Time: 0,
            NumSendCycles: send,
            NumReceiveCycles: receive,
            TempParamUpdate: 0,
            TxFlag: PDU_FLAG_DATA {
                NumFlagBytes: 0,
                pFlagData: ptr::null_mut(),
            },
            NumPossibleExpectedResponses: 0,
            pExpectedResponseArray: ptr::null_mut(),
        };
        assert_eq!(cycles(None), Cycles::SendReceive);
        assert_eq!(cycles(Some(&control(0, 0))), Cycles::NoSend);
        assert_eq!(cycles(Some(&control(0, 1))), Cycles::NoSend);
        assert_eq!(cycles(Some(&control(1, 0))), Cycles::SendOnly);
        assert_eq!(cycles(Some(&control(1, 1))), Cycles::SendReceive);
        assert_eq!(cycles(Some(&control(1, -2))), Cycles::SendReceive);
    }
}

#[cfg(test)]
mod delivery_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static ENTERED: AtomicBool = AtomicBool::new(false);
    static RELEASE: Mutex<bool> = Mutex::new(false);
    static RELEASED: Condvar = Condvar::new();

    /// Counts the call, then blocks it until the test releases it.
    fn block() {
        CALLS.fetch_add(1, Ordering::SeqCst);
        ENTERED.store(true, Ordering::SeqCst);
        let mut released = RELEASE.lock().unwrap();
        while !*released {
            released = RELEASED.wait(released).unwrap();
        }
    }

    #[cfg(not(all(windows, target_arch = "x86")))]
    unsafe extern "C" fn blocking_callback(
        _event_type: T_PDU_EVT_DATA,
        _h_mod: UNUM32,
        _h_cll: UNUM32,
        _cll_tag: *mut c_void,
        _api_tag: *mut c_void,
    ) {
        block();
    }

    #[cfg(all(windows, target_arch = "x86"))]
    unsafe extern "stdcall" fn blocking_callback(
        _event_type: T_PDU_EVT_DATA,
        _h_mod: UNUM32,
        _h_cll: UNUM32,
        _cll_tag: *mut c_void,
        _api_tag: *mut c_void,
    ) {
        block();
    }

    /// `PDUDestruct` returns only after a running callback has returned, and no callback runs
    /// after it (ADR-299): the caller frees the API tag and may unload the library then.
    #[test]
    fn the_delivery_thread_is_told_apart_from_its_callers() {
        static HANDLE: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
        let (sent, seen) = std::sync::mpsc::channel();
        let mut handle = HANDLE.lock().unwrap();
        *handle = Some(std::thread::spawn(move || {
            let handle = HANDLE.lock().unwrap();
            sent.send(is_current_thread(handle.as_ref().unwrap()))
                .unwrap();
        }));
        assert!(!is_current_thread(handle.as_ref().unwrap()));
        drop(handle);
        assert!(seen.recv().unwrap(), "the thread itself");
        HANDLE.lock().unwrap().take().unwrap().join().unwrap();
    }

    #[test]
    fn destruct_waits_for_a_running_callback_and_stops_the_rest() {
        mock_reset();
        unsafe {
            assert_eq!(
                PDUConstruct(ptr::null_mut(), ptr::dangling_mut::<c_void>()),
                no_error()
            );
            assert_eq!(
                PDURegisterEventCallback(
                    MOCK_MODULE_HANDLE,
                    MOCK_LOGICAL_LINK_HANDLE,
                    Some(blocking_callback),
                ),
                no_error()
            );
        }
        {
            let mut guard = state().lock().unwrap();
            let command = serde_json::from_str(r#"{"attach":{}}"#).unwrap();
            apply_sim_ecu_command(&mut guard, command).unwrap();
            let delay = r#"{"inject_fault":{"fault":{"delay_response":{"ms":2000}}}}"#;
            apply_sim_ecu_command(&mut guard, serde_json::from_str(delay).unwrap()).unwrap();
        }
        let mut request = [0x22u8, 0xF1, 0x90];
        let mut handle = 0;
        unsafe {
            PDUStartComPrimitive(
                MOCK_MODULE_HANDLE,
                MOCK_LOGICAL_LINK_HANDLE,
                E_PDU_COPT::PDU_COPT_SENDRECV,
                request.len() as UNUM32,
                request.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut handle,
            );
        }
        // The EXECUTING item's callback is now blocked; the response is due in 2 s.
        let started = Instant::now();
        while !ENTERED.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(5), "no callback");
            std::thread::sleep(Duration::from_millis(1));
        }
        let destructed = std::sync::Arc::new(AtomicBool::new(false));
        let destruct = {
            let destructed = destructed.clone();
            std::thread::spawn(move || {
                unsafe { PDUDestruct() };
                destructed.store(true, Ordering::SeqCst);
            })
        };
        // Released only once `PDUDestruct` has asked the thread to stop.
        let started = Instant::now();
        while !state().lock().unwrap().stop_delivery {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "PDUDestruct did not run"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !destructed.load(Ordering::SeqCst),
            "PDUDestruct returned while a callback ran"
        );
        *RELEASE.lock().unwrap() = true;
        RELEASED.notify_all();
        destruct.join().unwrap();
        assert!(destructed.load(Ordering::SeqCst));
        // The thread has ended, so the response and FINISHED items never reach the callback.
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        mock_reset();
    }
}
