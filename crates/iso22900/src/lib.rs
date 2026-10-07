#![allow(unsafe_code)]
//! Rust wrapper for ISO 22900-2 D-PDU API.
//!
//! This crate provides typed wrappers over the native D-PDU C API, including
//! module/link lifecycle, communication primitives, event handling, and IOCTL.

use std::{
    ffi::{CString, c_void},
    fmt,
    mem::MaybeUninit,
    path::Path,
    ptr::{self},
};

use tracing::{debug, info};

use iso22900_sys::bindings::{
    DPduApiSys, PDU_DATA_ITEM, PDU_MODULE_DATA, PDU_MODULE_ITEM, PDU_VERSION_DATA, T_PDU_COPT,
    T_PDU_CPST, T_PDU_ERR_EVT, T_PDU_EVT_DATA, T_PDU_FILTER, T_PDU_OBJT, T_PDU_QUEUE_MODE, UNUM32,
};

mod decode;
mod encode;
mod events;
mod item;
mod safety;

// pub use removed: resolve_library_path_from_rdf is now in iso22900-registry

pub use iso22900_sys::bindings::{
    E_PDU_COPT, E_PDU_CPST, E_PDU_ERR_EVT, E_PDU_ERROR, E_PDU_EVT_DATA, E_PDU_FILTER, E_PDU_INFO,
    E_PDU_IT, E_PDU_OBJT, E_PDU_PC, E_PDU_PT, E_PDU_QUEUE_MODE, E_PDU_STATUS, PDU_HANDLE_UNDEF,
    PDU_ID_UNDEF, T_PDU_PC, T_PDU_PT,
};
pub use item::*;

use decode::decode_version_info;
use encode::{
    EncodedComParam, EncodedComPrimitiveControl, EncodedFlagData, EncodedResourceDescriptor,
    EncodedUniqueResponseTable,
};
use events::{ApiTagContext, event_callback_trampoline};
use safety::check;

/// Opaque handle for an opened VCI module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleHandle(pub UNUM32);

/// Opaque handle for a communication logical link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComLogicalLinkHandle(pub UNUM32);

/// Opaque handle for an active communication primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComPrimitiveHandle(pub UNUM32);

/// Resource identifier returned by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(pub UNUM32);

/// Generic object identifier returned by `get_object_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub UNUM32);

/// Generic status code wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PduStatus(pub UNUM32);

/// Parameter class wrapper used for COM parameter metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamClass(pub T_PDU_PC);

impl ParamClass {
    /// Timing-related parameters.
    pub const TIMING: Self = Self(T_PDU_PC::PDU_PC_TIMING);
    /// Initialization parameters.
    pub const INIT: Self = Self(T_PDU_PC::PDU_PC_INIT);
    /// Communication parameters.
    pub const COM: Self = Self(T_PDU_PC::PDU_PC_COM);
    /// Error-handling parameters.
    pub const ERRHDL: Self = Self(T_PDU_PC::PDU_PC_ERRHDL);
    /// Bus-type parameters.
    pub const BUSTYPE: Self = Self(T_PDU_PC::PDU_PC_BUSTYPE);
    /// Unique-identifier parameters.
    pub const UNIQUE_ID: Self = Self(T_PDU_PC::PDU_PC_UNIQUE_ID);
    /// Tester-present behavior parameters.
    pub const TESTER_PRESENT: Self = Self(T_PDU_PC::PDU_PC_TESTER_PRESENT);

    fn as_raw(self) -> T_PDU_PC {
        self.0
    }
}

/// Parameter data type wrapper used for COM parameter encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamDataType(pub T_PDU_PT);

impl ParamDataType {
    /// Unsigned 8-bit value.
    pub const U8: Self = Self(T_PDU_PT::PDU_PT_UNUM8);
    /// Signed 8-bit value.
    pub const I8: Self = Self(T_PDU_PT::PDU_PT_SNUM8);
    /// Unsigned 16-bit value.
    pub const U16: Self = Self(T_PDU_PT::PDU_PT_UNUM16);
    /// Signed 16-bit value.
    pub const I16: Self = Self(T_PDU_PT::PDU_PT_SNUM16);
    /// Unsigned 32-bit value.
    pub const U32: Self = Self(T_PDU_PT::PDU_PT_UNUM32);
    /// Signed 32-bit value.
    pub const I32: Self = Self(T_PDU_PT::PDU_PT_SNUM32);
    /// Variable byte-field value.
    pub const BYTE_FIELD: Self = Self(T_PDU_PT::PDU_PT_BYTEFIELD);
    /// Structured field value.
    pub const STRUCT_FIELD: Self = Self(T_PDU_PT::PDU_PT_STRUCTFIELD);
    /// Long-field value.
    pub const LONG_FIELD: Self = Self(T_PDU_PT::PDU_PT_LONGFIELD);

    fn as_raw(self) -> T_PDU_PT {
        self.0
    }
}

/// Communication primitive type wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComPrimitiveType(pub T_PDU_COPT);

impl ComPrimitiveType {
    fn as_raw(self) -> T_PDU_COPT {
        self.0
    }
}

/// Object type wrapper used by `get_object_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectType(pub T_PDU_OBJT);

impl ObjectType {
    fn as_raw(self) -> T_PDU_OBJT {
        self.0
    }
}

/// Error event code wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorEventCode(pub UNUM32);

/// IO control item type wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IoCtlItemType(pub UNUM32);

/// Event item type wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventItemType(pub UNUM32);

/// Notification kind delivered by registered event callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventNotificationType {
    DataAvailable,
    DataLost,
    Unknown(T_PDU_EVT_DATA),
}

impl EventNotificationType {
    fn from_raw(value: T_PDU_EVT_DATA) -> Self {
        match value {
            T_PDU_EVT_DATA::PDU_EVT_DATA_AVAILABLE => Self::DataAvailable,
            T_PDU_EVT_DATA::PDU_EVT_DATA_LOST => Self::DataLost,
            _ => Self::Unknown(value),
        }
    }
}

/// High-level callback payload for link-level events.
///
/// Carries no `cll_tag` field: ADR-204 removed the gRPC-facing
/// `CreateComLogicalLinkRequest.cll_tag` after establishing that no
/// downstream code (in this crate or `iso22900-service`) ever read the
/// native echo, so the field was tracked here purely for a consumer that
/// never existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventNotification {
    pub event_type: EventNotificationType,
    pub module_handle: ModuleHandle,
    pub logical_link_handle: ComLogicalLinkHandle,
}

/// Version and build metadata returned by `PDUGetVersion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub mvci_part_1_standard_version: UNUM32,
    pub mvci_part_2_standard_version: UNUM32,
    pub hardware_serial_number: UNUM32,
    pub hardware_name: String,
    pub hardware_version: UNUM32,
    pub hardware_date: UNUM32,
    pub hardware_interface: UNUM32,
    pub firmware_name: String,
    pub firmware_version: UNUM32,
    pub firmware_date: UNUM32,
    pub vendor_name: String,
    pub api_software_name: String,
    pub api_software_version: UNUM32,
    pub api_software_date: UNUM32,
}

/// Basic module information returned by module-item decoders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub module_type_id: ObjectId,
    pub handle: ModuleHandle,
    pub vendor_module_name: Option<String>,
    pub vendor_additional_info: Option<String>,
    pub status: PduStatus,
}

/// Status entry pairing a module/resource with current status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceStatusEntry {
    pub module_handle: ModuleHandle,
    pub resource_id: ResourceId,
    pub status: PduStatus,
}

/// One physical pin descriptor used in a resource description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinData {
    pub dlc_pin_number: UNUM32,
    pub dlc_pin_type_id: ObjectId,
}

/// Resource descriptor used to request matching resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDescriptor {
    pub bus_type_id: ObjectId,
    pub protocol_id: ObjectId,
    pub pins: Vec<PinData>,
}

/// Resource selection by explicit id or by descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    ById(ResourceId),
    ByDescriptor(ResourceDescriptor),
}

/// Resource identifiers grouped per module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleResourceIds {
    pub module_handle: ModuleHandle,
    pub resource_ids: Vec<ResourceId>,
}

/// A module/resource conflict entry returned by conflict queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictingResource {
    pub module_handle: ModuleHandle,
    pub resource_id: ResourceId,
}

/// Opaque byte flags used by API calls.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FlagData {
    pub bytes: Vec<u8>,
}

/// Typed COM parameter value representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComParamValue {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    Bytes(Vec<u8>),
    Longs(Vec<u32>),
    StructField {
        struct_type: T_PDU_CPST,
        encoding: StructFieldEncoding,
    },
    Unsupported {
        data_type: ParamDataType,
    },
}

/// One COM parameter entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComParam {
    pub id: ObjectId,
    pub data_type: ParamDataType,
    pub class: ParamClass,
    pub value: ComParamValue,
}

/// Encoded bytes for structured COM parameter payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructFieldEncoding {
    pub entry_size: usize,
    pub bytes: Vec<u8>,
}

/// Expected-response matching rule for communication primitives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedResponse {
    pub response_type: UNUM32,
    pub acceptance_id: UNUM32,
    pub mask: Vec<u8>,
    pub pattern: Vec<u8>,
    pub unique_response_ids: Vec<UNUM32>,
}

/// Control block used when starting communication primitives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComPrimitiveControl {
    pub time: UNUM32,
    pub send_cycles: i32,
    pub receive_cycles: i32,
    pub temp_param_update: UNUM32,
    pub tx_flags: FlagData,
    pub expected_responses: Vec<ExpectedResponse>,
}

/// Primitive status triple returned by `get_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    pub status: PduStatus,
    pub timestamp: UNUM32,
    pub extra_info: UNUM32,
}

/// Last-error details returned by `get_last_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastErrorInfo {
    pub error_code: ErrorEventCode,
    pub com_primitive_handle: ComPrimitiveHandle,
    pub timestamp: UNUM32,
    pub extra_error_info: UNUM32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultData {
    pub rx_flag: Vec<u8>,
    pub unique_resp_identifier: UNUM32,
    pub acceptance_id: UNUM32,
    pub timestamp_flags: Vec<u8>,
    pub tx_msg_done_timestamp: UNUM32,
    pub start_msg_timestamp: UNUM32,
    pub header_bytes: Vec<u8>,
    pub footer_bytes: Vec<u8>,
    pub data_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusData {
    pub status: PduStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorData {
    pub error_code_id: ErrorEventCode,
    pub extra_error_info_id: UNUM32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoData {
    pub info_code: UNUM32,
    pub extra_info_data: UNUM32,
}

/// Typed event payload decoded from `PDUGetEventItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventItemData {
    Result(ResultData),
    Status(StatusData),
    Error(ErrorData),
    Info(InfoData),
    Unknown(EventItemType),
}

/// Event item decoded from event queue entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventItem {
    pub item_type: EventItemType,
    pub com_primitive_handle: ComPrimitiveHandle,
    pub cop_tag: usize,
    pub timestamp: UNUM32,
    pub data: EventItemData,
}

/// One unique-response table entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueResponseEntry {
    pub unique_response_id: UNUM32,
    pub params: Vec<ComParam>,
}

/// IOCTL payload for programming-voltage requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoProgVoltageData {
    pub prog_voltage_mv: UNUM32,
    pub pin_on_dlc: UNUM32,
}

/// Generic IOCTL byte-array payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoByteArrayData {
    pub data: Vec<u8>,
}

/// IOCTL payload describing one filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoFilterData {
    pub filter_type: T_PDU_FILTER,
    pub filter_number: UNUM32,
    pub filter_compare_size: UNUM32,
    pub filter_mask_message: Vec<u8>,
    pub filter_pattern_message: Vec<u8>,
}

/// IOCTL payload describing multiple filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoFilterListData {
    pub filters: Vec<IoFilterData>,
}

/// IOCTL payload for event-queue settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoEventQueuePropertyData {
    pub queue_size: UNUM32,
    pub queue_mode: T_PDU_QUEUE_MODE,
}

/// IP address entry used by vehicle discovery payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoIpAddrInfo {
    pub ip_version: UNUM32,
    pub address: Vec<u8>,
}

/// IOCTL payload for vehicle ID request configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoVehicleIdRequestData {
    pub preselection_mode: UNUM32,
    pub preselection_value: Option<String>,
    pub combination_mode: UNUM32,
    pub vehicle_discovery_time: UNUM32,
    pub destination_addresses: Vec<IoIpAddrInfo>,
}

/// IOCTL payload for Ethernet switch state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoEthSwitchStateData {
    pub ethernet_sense_state: UNUM32,
    pub ethernet_act_pin_number: UNUM32,
}

/// IOCTL payload for DoIP entity addressing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoEntityAddressData {
    pub logical_address: UNUM32,
    pub doip_ctrl_timeout: UNUM32,
}

/// IOCTL payload for entity status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoEntityStatusData {
    pub entity_type: UNUM32,
    pub tcp_clients_max: UNUM32,
    pub tcp_clients: UNUM32,
    pub max_data_size: UNUM32,
}

/// One TLS certificate buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoTlsCertData {
    pub cert_buffer: Vec<u8>,
}

/// IOCTL payload containing one or more certificate chains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoTlsCertificateData {
    pub cert_chains: Vec<IoTlsCertData>,
}

/// High-level error type for D-PDU operations.
#[derive(Debug)]
pub enum DPduApiError {
    LibLoading(iso22900_sys::libloading::Error),
    PduError(UNUM32),
    InvalidShortName,
    NullPointer(&'static str),
    Unsupported(&'static str),
    CallbackRegistryPoisoned,
}

impl fmt::Display for DPduApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LibLoading(error) => write!(f, "library loading error: {error}"),
            Self::PduError(code) => write!(f, "PDU error code: {code:#010x}"),
            Self::InvalidShortName => write!(f, "short name contains an interior NUL byte"),
            Self::NullPointer(name) => write!(f, "received null pointer for {name}"),
            Self::Unsupported(name) => write!(f, "unsupported ISO22900 payload: {name}"),
            Self::CallbackRegistryPoisoned => write!(f, "callback registry is poisoned"),
        }
    }
}

impl std::error::Error for DPduApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LibLoading(error) => Some(error),
            Self::PduError(_)
            | Self::InvalidShortName
            | Self::NullPointer(_)
            | Self::Unsupported(_)
            | Self::CallbackRegistryPoisoned => None,
        }
    }
}

impl From<iso22900_sys::libloading::Error> for DPduApiError {
    fn from(value: iso22900_sys::libloading::Error) -> Self {
        Self::LibLoading(value)
    }
}

pub struct DPduApi {
    sys: DPduApiSys,
    api_tag_context: Box<ApiTagContext>,
}

impl DPduApi {
    /// Loads the D-PDU shared library and constructs the API context.
    pub fn new(path: &Path) -> Result<Self, DPduApiError> {
        info!(path = %path.display(), "loading D-PDU API library");
        #[cfg(windows)]
        let sys = unsafe {
            DPduApiSys::from_library(
                iso22900_sys::libloading::os::windows::Library::load_with_flags(
                    path,
                    iso22900_sys::libloading::os::windows::LOAD_LIBRARY_SEARCH_DEFAULT_DIRS
                        | iso22900_sys::libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
                )?,
            )
        }?;
        #[cfg(not(windows))]
        let sys = unsafe { DPduApiSys::new(path) }?;

        let mut api_tag_context = Box::new(ApiTagContext::new());
        let context_ptr = (&mut *api_tag_context as *mut ApiTagContext).cast::<c_void>();

        check(unsafe { sys.PDUConstruct(ptr::null_mut(), context_ptr) })?;
        debug!(path = %path.display(), "D-PDU API library loaded (PDUConstruct OK)");
        Ok(Self {
            sys,
            api_tag_context,
        })
    }

    /// Connects to a module handle.
    pub fn module_connect(&self, module_handle: ModuleHandle) -> Result<(), DPduApiError> {
        info!(module_handle = module_handle.0, "PDUModuleConnect");
        check(unsafe { self.sys.PDUModuleConnect(module_handle.0) })
    }

    /// Disconnects a module and clears callbacks registered for it.
    pub fn module_disconnect(&self, module_handle: ModuleHandle) -> Result<(), DPduApiError> {
        info!(module_handle = module_handle.0, "PDUModuleDisconnect");
        self.cleanup_event_callbacks_for_module(module_handle);
        check(unsafe { self.sys.PDUModuleDisconnect(module_handle.0) })
    }

    /// Reads the module timestamp counter.
    pub fn get_timestamp(&self, module_handle: ModuleHandle) -> Result<UNUM32, DPduApiError> {
        let mut timestamp = 0;
        check(unsafe { self.sys.PDUGetTimestamp(module_handle.0, &mut timestamp) })?;
        Ok(timestamp)
    }

    /// Reads module/API version metadata.
    pub fn get_version(&self, module_handle: ModuleHandle) -> Result<VersionInfo, DPduApiError> {
        let mut version = unsafe { MaybeUninit::<PDU_VERSION_DATA>::zeroed().assume_init() };
        check(unsafe { self.sys.PDUGetVersion(module_handle.0, &mut version) })?;
        Ok(decode_version_info(&version))
    }

    /// Returns available module IDs and metadata.
    pub fn get_module_ids(&self) -> Result<ApiItem<'_, BorrowedModuleItem>, DPduApiError> {
        let mut item_ptr = ptr::null_mut();
        check(unsafe { self.sys.PDUGetModuleIds(&mut item_ptr) })?;
        // SAFETY: on success the library returns an item of this type, or null (rejected).
        unsafe { ApiItem::from_raw(&self.sys, item_ptr.cast()) }
    }

    /// Queries resource status for a list of `(module, resource)` pairs.
    pub fn get_resource_status<C: IntoIterator<Item = (ModuleHandle, ResourceId)>>(
        &self,
        entries: C,
    ) -> Result<OwnedResourceStatusItem, DPduApiError> {
        let mut item = OwnedResourceStatusItem::from_entries(entries);
        check(unsafe { self.sys.PDUGetResourceStatus(item.as_mut_raw()) })?;
        Ok(item)
    }

    /// Resolves matching resource IDs for a descriptor on a module.
    pub fn get_resource_ids(
        &self,
        module_handle: ModuleHandle,
        resource_descriptor: &ResourceDescriptor,
    ) -> Result<ApiItem<'_, BorrowedResourceIdItem>, DPduApiError> {
        let mut descriptor = EncodedResourceDescriptor::new(resource_descriptor);
        let mut item_ptr = ptr::null_mut();
        check(unsafe {
            self.sys
                .PDUGetResourceIds(module_handle.0, &mut descriptor.ffi, &mut item_ptr)
        })?;
        // SAFETY: on success the library returns an item of this type, or null (rejected).
        unsafe { ApiItem::from_raw(&self.sys, item_ptr.cast()) }
    }

    /// Returns modules currently conflicting with a resource.
    pub fn get_conflicting_resources(
        &self,
        resource_id: ResourceId,
        modules: &[ModuleHandle],
    ) -> Result<ApiItem<'_, BorrowedResourceConflictItem>, DPduApiError> {
        let mut module_data: Vec<PDU_MODULE_DATA> = Vec::with_capacity(modules.len());
        for module in modules {
            module_data.push(PDU_MODULE_DATA {
                ModuleTypeId: 0,
                hMod: module.0,
                pVendorModuleName: ptr::null_mut(),
                pVendorAdditionalInfo: ptr::null_mut(),
                ModuleStatus: E_PDU_STATUS::PDU_MODST_NOT_AVAIL,
            });
        }
        let mut module_item = PDU_MODULE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_MODULE_ID,
            NumEntries: module_data.len() as UNUM32,
            pModuleData: if module_data.is_empty() {
                ptr::null_mut()
            } else {
                module_data.as_mut_ptr()
            },
        };
        let mut item_ptr = ptr::null_mut();
        check(unsafe {
            self.sys
                .PDUGetConflictingResources(resource_id.0, &mut module_item, &mut item_ptr)
        })?;
        // SAFETY: on success the library returns an item of this type, or null (rejected).
        unsafe { ApiItem::from_raw(&self.sys, item_ptr.cast()) }
    }

    /// Creates a communication logical link using a resource id or descriptor.
    pub fn create_com_logical_link(
        &self,
        module_handle: ModuleHandle,
        resource: Resource,
        cll_tag: Option<usize>,
        creation_flags: FlagData,
    ) -> Result<ComLogicalLinkHandle, DPduApiError> {
        let (mut resource_descriptor, resource_id) = match resource {
            Resource::ById(id) => (None, Some(id)),
            Resource::ByDescriptor(descriptor) => {
                (Some(EncodedResourceDescriptor::new(&descriptor)), None)
            }
        };
        let mut flags = EncodedFlagData::from_bytes(creation_flags.bytes);
        let mut handle = 0;
        check(unsafe {
            self.sys.PDUCreateComLogicalLink(
                module_handle.0,
                resource_descriptor
                    .as_mut()
                    .map_or(ptr::null_mut(), |d| &mut d.ffi),
                resource_id.map_or(PDU_ID_UNDEF, |id| id.0),
                cll_tag
                    .map(|tag| tag as *mut c_void)
                    .unwrap_or(ptr::null_mut()),
                &mut handle,
                &mut flags.ffi,
            )
        })?;
        debug!(
            module_handle = module_handle.0,
            cll_handle = handle,
            "PDUCreateComLogicalLink"
        );
        Ok(ComLogicalLinkHandle(handle))
    }

    /// Destroys a communication logical link and removes its callback registration.
    pub fn destroy_com_logical_link(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) -> Result<(), DPduApiError> {
        debug!(
            module_handle = module_handle.0,
            cll_handle = logical_link_handle.0,
            "PDUDestroyComLogicalLink"
        );
        self.cleanup_event_callback_for_link(module_handle, logical_link_handle);
        check(unsafe {
            self.sys
                .PDUDestroyComLogicalLink(module_handle.0, logical_link_handle.0)
        })
    }

    /// Connects a communication logical link.
    pub fn connect_com_logical_link(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) -> Result<(), DPduApiError> {
        info!(
            module_handle = module_handle.0,
            cll_handle = logical_link_handle.0,
            "PDUConnect"
        );
        check(unsafe { self.sys.PDUConnect(module_handle.0, logical_link_handle.0) })
    }

    /// Disconnects a communication logical link.
    pub fn disconnect_com_logical_link(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) -> Result<(), DPduApiError> {
        info!(
            module_handle = module_handle.0,
            cll_handle = logical_link_handle.0,
            "PDUDisconnect"
        );
        check(unsafe {
            self.sys
                .PDUDisconnect(module_handle.0, logical_link_handle.0)
        })
    }

    /// Applies a resource lock mask to a link.
    pub fn lock_resource(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        lock_mask: UNUM32,
    ) -> Result<(), DPduApiError> {
        check(unsafe {
            self.sys
                .PDULockResource(module_handle.0, logical_link_handle.0, lock_mask)
        })
    }

    /// Releases a resource lock mask from a link.
    pub fn unlock_resource(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        lock_mask: UNUM32,
    ) -> Result<(), DPduApiError> {
        check(unsafe {
            self.sys
                .PDUUnlockResource(module_handle.0, logical_link_handle.0, lock_mask)
        })
    }

    /// Reads a COM parameter by id.
    pub fn get_com_param(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        param_id: ObjectId,
    ) -> Result<ApiItem<'_, BorrowedParamItem>, DPduApiError> {
        let mut item_ptr = ptr::null_mut();
        check(unsafe {
            self.sys.PDUGetComParam(
                module_handle.0,
                logical_link_handle.0,
                param_id.0,
                &mut item_ptr,
            )
        })?;
        // SAFETY: on success the library returns an item of this type, or null (rejected).
        unsafe { ApiItem::from_raw(&self.sys, item_ptr.cast()) }
    }

    /// Writes a COM parameter.
    pub fn set_com_param(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        param: ComParam,
    ) -> Result<(), DPduApiError> {
        let mut encoded = EncodedComParam::new_owned(param)?;
        check(unsafe {
            self.sys
                .PDUSetComParam(module_handle.0, logical_link_handle.0, &mut encoded.ffi)
        })
    }

    /// Starts a communication primitive and returns its handle.
    pub fn start_com_primitive(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        primitive_type: ComPrimitiveType,
        mut cop_data: Vec<u8>,
        control: ComPrimitiveControl,
        cop_tag: usize,
    ) -> Result<ComPrimitiveHandle, DPduApiError> {
        let mut control = EncodedComPrimitiveControl::new_owned(control);
        let mut handle = 0;
        check(unsafe {
            self.sys.PDUStartComPrimitive(
                module_handle.0,
                logical_link_handle.0,
                primitive_type.as_raw(),
                cop_data.len() as UNUM32,
                if cop_data.is_empty() {
                    ptr::null_mut()
                } else {
                    cop_data.as_mut_ptr()
                },
                &mut control.ffi,
                cop_tag as *mut c_void,
                &mut handle,
            )
        })?;
        Ok(ComPrimitiveHandle(handle))
    }

    /// Cancels a running communication primitive.
    pub fn cancel_com_primitive(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        primitive_handle: ComPrimitiveHandle,
    ) -> Result<(), DPduApiError> {
        check(unsafe {
            self.sys.PDUCancelComPrimitive(
                module_handle.0,
                logical_link_handle.0,
                primitive_handle.0,
            )
        })
    }

    /// Returns primitive execution status and timing info.
    pub fn get_status(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        primitive_handle: ComPrimitiveHandle,
    ) -> Result<StatusInfo, DPduApiError> {
        let mut status = E_PDU_STATUS::PDU_COPST_IDLE;
        let mut timestamp = 0;
        let mut extra_info = 0;
        check(unsafe {
            self.sys.PDUGetStatus(
                module_handle.0,
                logical_link_handle.0,
                primitive_handle.0,
                &mut status,
                &mut timestamp,
                &mut extra_info,
            )
        })?;
        Ok(StatusInfo {
            status: PduStatus(status.0 as UNUM32),
            timestamp,
            extra_info,
        })
    }

    /// Returns last error details for a logical link.
    pub fn get_last_error(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) -> Result<LastErrorInfo, DPduApiError> {
        let mut error_code = T_PDU_ERR_EVT::PDU_ERR_EVT_NOERROR;
        let mut primitive_handle = 0;
        let mut timestamp = 0;
        let mut extra_error_info = 0;
        check(unsafe {
            self.sys.PDUGetLastError(
                module_handle.0,
                logical_link_handle.0,
                &mut error_code,
                &mut primitive_handle,
                &mut timestamp,
                &mut extra_error_info,
            )
        })?;
        Ok(LastErrorInfo {
            error_code: ErrorEventCode(error_code.0 as UNUM32),
            com_primitive_handle: ComPrimitiveHandle(primitive_handle),
            timestamp,
            extra_error_info,
        })
    }

    /// Pops one event item from the logical-link event queue.
    pub fn get_event_item(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) -> Result<ApiItem<'_, BorrowedEventItem>, DPduApiError> {
        let mut item_ptr = ptr::null_mut();
        check(unsafe {
            self.sys
                .PDUGetEventItem(module_handle.0, logical_link_handle.0, &mut item_ptr)
        })?;
        // SAFETY: on success the library returns an item of this type, or null (rejected).
        unsafe { ApiItem::from_raw(&self.sys, item_ptr.cast()) }
    }

    /// Registers an event callback for a specific `(module, link)` pair.
    pub fn register_event_callback<F>(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        callback: F,
    ) -> Result<(), DPduApiError>
    where
        F: FnMut(EventNotification) + Send + 'static,
    {
        let key = (module_handle, logical_link_handle);
        self.api_tag_context
            .callbacks
            .lock()
            .map_err(|_| DPduApiError::CallbackRegistryPoisoned)?
            .insert(key, Box::new(callback));

        let result = check(unsafe {
            self.sys.PDURegisterEventCallback(
                module_handle.0,
                logical_link_handle.0,
                Some(event_callback_trampoline),
            )
        });

        if result.is_err() {
            let _ = self
                .api_tag_context
                .callbacks
                .lock()
                .map_err(|_| DPduApiError::CallbackRegistryPoisoned)?
                .remove(&key);
        }

        result
    }

    fn cleanup_event_callback_for_link(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) {
        let key = (module_handle, logical_link_handle);
        let _ = self
            .api_tag_context
            .callbacks
            .lock()
            .map(|mut callbacks| callbacks.remove(&key));
    }

    fn cleanup_event_callbacks_for_module(&self, module_handle: ModuleHandle) {
        let _ = self
            .api_tag_context
            .callbacks
            .lock()
            .map(|mut callbacks| callbacks.retain(|(h_mod, _), _| *h_mod != module_handle));
    }

    /// Unregisters a previously registered event callback.
    pub fn unregister_event_callback(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) -> Result<(), DPduApiError> {
        self.api_tag_context
            .callbacks
            .lock()
            .map_err(|_| DPduApiError::CallbackRegistryPoisoned)?
            .remove(&(module_handle, logical_link_handle));
        check(unsafe {
            self.sys
                .PDURegisterEventCallback(module_handle.0, logical_link_handle.0, None)
        })
    }

    /// Resolves an object ID from object type and short name.
    pub fn get_object_id(
        &self,
        object_type: ObjectType,
        short_name: &str,
    ) -> Result<ObjectId, DPduApiError> {
        let short_name = CString::new(short_name).map_err(|_| DPduApiError::InvalidShortName)?;
        let mut object_id = 0;
        check(unsafe {
            self.sys.PDUGetObjectId(
                object_type.as_raw(),
                short_name.as_ptr().cast_mut(),
                &mut object_id,
            )
        })?;
        Ok(ObjectId(object_id))
    }

    /// Reads the current unique-response-id table.
    pub fn get_unique_resp_id_table(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
    ) -> Result<ApiItem<'_, BorrowedUniqueRespIdTableItem>, DPduApiError> {
        let mut item_ptr = ptr::null_mut();
        check(unsafe {
            self.sys
                .PDUGetUniqueRespIdTable(module_handle.0, logical_link_handle.0, &mut item_ptr)
        })?;
        // SAFETY: on success the library returns an item of this type, or null (rejected).
        unsafe { ApiItem::from_raw(&self.sys, item_ptr.cast()) }
    }

    /// Replaces the unique-response-id table with provided entries.
    pub fn set_unique_resp_id_table(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        entries: Vec<UniqueResponseEntry>,
    ) -> Result<(), DPduApiError> {
        let mut table = EncodedUniqueResponseTable::new_owned(entries)?;
        check(unsafe {
            self.sys
                .PDUSetUniqueRespIdTable(module_handle.0, logical_link_handle.0, &mut table.ffi)
        })
    }

    /// Executes an IOCTL and returns output item data.
    pub fn io_ctl_with_output(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        io_ctl_command_id: UNUM32,
        input: Option<&BorrowedDataItem>,
    ) -> Result<ApiItem<'_, BorrowedDataItem>, DPduApiError> {
        let input_ptr = input.map_or(ptr::null_mut(), |item| {
            item as *const BorrowedDataItem as *mut PDU_DATA_ITEM
        });

        let mut output_ptr = ptr::null_mut();
        check(unsafe {
            self.sys.PDUIoCtl(
                module_handle.0,
                logical_link_handle.0,
                io_ctl_command_id,
                input_ptr,
                &mut output_ptr,
            )
        })?;
        // SAFETY: on success the library returns an item of this type, or null (rejected).
        unsafe { ApiItem::from_raw(&self.sys, output_ptr.cast()) }
    }

    /// Executes an IOCTL that does not return output item data.
    pub fn io_ctl_without_output(
        &self,
        module_handle: ModuleHandle,
        logical_link_handle: ComLogicalLinkHandle,
        io_ctl_command_id: UNUM32,
        input: Option<&BorrowedDataItem>,
    ) -> Result<(), DPduApiError> {
        let input_ptr = input.map_or(ptr::null_mut(), |item| {
            item as *const BorrowedDataItem as *mut PDU_DATA_ITEM
        });
        check(unsafe {
            self.sys.PDUIoCtl(
                module_handle.0,
                logical_link_handle.0,
                io_ctl_command_id,
                input_ptr,
                ptr::null_mut(),
            )
        })
    }
}

impl Drop for DPduApi {
    fn drop(&mut self) {
        unsafe {
            let _ = self.sys.PDUDestruct();
        }
    }
}
