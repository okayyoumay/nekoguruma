#![allow(clippy::too_many_arguments)]
#![allow(unsafe_code)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use iso22900_sys::bindings::*;

const MOCK_MODULE_HANDLE: UNUM32 = 1001;
const MOCK_RESOURCE_ID: UNUM32 = 2001;
const MOCK_LOGICAL_LINK_HANDLE: UNUM32 = 3001;
const MOCK_COM_PRIMITIVE_HANDLE: UNUM32 = 4001;
const MOCK_TIMESTAMP: UNUM32 = 4242;
const MOCK_VERSION_TAG: &[u8] = b"iso22900-mock\0";
const MOCK_VEHICLE_NAME: &[u8] = b"mock-vehicle\0";

/// IOCTL command id for which `PDUIoCtl` reports success without filling in the output item,
/// so tests can check how the wrapper handles a library that returns a null item.
pub const MOCK_IOCTL_NULL_OUTPUT: UNUM32 = 0x7FFF_FF00;

#[derive(Clone, Copy)]
struct PendingEvent {
    h_mod: UNUM32,
    h_cll: UNUM32,
    h_cop: UNUM32,
    tag: usize,
    timestamp: UNUM32,
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
        NumDataBytes: 0,
        pDataBytes: ptr::null_mut(),
    });

    let data_ptr = Box::into_raw(result_data).cast::<c_void>();
    Box::into_raw(Box::new(PDU_EVENT_ITEM {
        ItemType: E_PDU_IT::PDU_IT_RESULT,
        hCop: event.h_cop,
        pCoPTag: event.tag as *mut c_void,
        Timestamp: event.timestamp,
        pData: data_ptr,
    }))
}

unsafe fn notify_data_available(callback: CALLBACKFNC, event: PendingEvent, api_tag: usize) {
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
    let (callback, api_tag) = {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.pending_events.push(event);
        (guard.callback, guard.api_tag)
    };

    unsafe { notify_data_available(callback, event, api_tag) };
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
    guard.constructed = true;
    guard.api_tag = p_api_tag as usize;
    guard.next_com_primitive_handle = MOCK_COM_PRIMITIVE_HANDLE;
    guard.pending_events.clear();
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

exported_fn!(PDUGetStatus(_h_mod: UNUM32, _h_cll: UNUM32, _h_cop: UNUM32, p_status_code: *mut T_PDU_STATUS, p_timestamp: *mut UNUM32, p_extra_info: *mut UNUM32) -> T_PDU_ERROR {
    if p_status_code.is_null() || p_timestamp.is_null() || p_extra_info.is_null() {
        return invalid_parameters();
    }

    {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.counters.get_status += 1;
    }

    unsafe {
        *p_status_code = E_PDU_STATUS::PDU_COPST_FINISHED;
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

exported_fn!(PDUStartComPrimitive(_h_mod: UNUM32, _h_cll: UNUM32, _cop_type: T_PDU_COPT, _cop_data_size: UNUM32, _p_cop_data: *mut UNUM8, _p_cop_ctrl_data: *mut PDU_COP_CTRL_DATA, p_cop_tag: *mut c_void, ph_cop: *mut UNUM32) -> T_PDU_ERROR {
    if ph_cop.is_null() {
        return invalid_parameters();
    }

    let handle = {
        let mut guard = state().lock().expect("mock state poisoned");
        guard.counters.start_com_primitive += 1;
        guard.next_primitive_handle()
    };

    unsafe {
        *ph_cop = handle;
    }

    let event = PendingEvent {
        h_mod: MOCK_MODULE_HANDLE,
        h_cll: MOCK_LOGICAL_LINK_HANDLE,
        h_cop: handle,
        tag: p_cop_tag as usize,
        timestamp: MOCK_TIMESTAMP,
    };
    unsafe { queue_event(event) };
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
        E_PDU_IT::PDU_IT_RESULT | E_PDU_IT::PDU_IT_STATUS | E_PDU_IT::PDU_IT_ERROR | E_PDU_IT::PDU_IT_INFO => {
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
        unsafe { notify_data_available(event_callback_function, event, api_tag) };
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
    *guard = MockState::default();
    no_error()
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

pub fn mock_root_definition_file_path() -> Result<PathBuf, std::io::Error> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_file = manifest_dir.join("mock_root_definition.xml");
    if !root_file.exists() {
        let lib_path = mock_library_path()?;
        let rdf_content = format!(
            r#"
<MVCI_PDU_APIS>
    <MVCI_PDU_API>
        <SHORT_NAME>TestLib</SHORT_NAME>
        <DESCRIPTION>Test library</DESCRIPTION>
        <SUPPLIER_NAME>TestSupplier</SUPPLIER_NAME>
        <LIBRARY_FILE URI="file:///{}"/>
    </MVCI_PDU_API>
</MVCI_PDU_APIS>
"#,
            lib_path.to_string_lossy()
        );
        // Write via a per-thread temp file and rename into place, since this file is
        // generated lazily and multiple test threads may race to create it concurrently;
        // a direct write() could leave concurrent readers seeing a truncated file.
        let temp_file = manifest_dir.join(format!(
            "mock_root_definition.xml.{:?}.tmp",
            std::thread::current().id()
        ));
        std::fs::write(&temp_file, rdf_content).expect("write test RDF native");
        std::fs::rename(&temp_file, &root_file).expect("rename test RDF into place");
    }
    if root_file.is_file() {
        Ok(root_file)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "could not find mock_root_definition.xml under {}",
                manifest_dir.display()
            ),
        ))
    }
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
    *guard = MockState::default();
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
}
