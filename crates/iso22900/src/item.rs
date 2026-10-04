use std::borrow::{Borrow, ToOwned};
use std::ffi::{CStr, CString, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;

use iso22900_sys::bindings::{
    CHAR8, DPduApiSys, E_PDU_CPST, E_PDU_IT, E_PDU_PT, PDU_DATA_ITEM, PDU_ECU_UNIQUE_RESP_DATA,
    PDU_ERROR_DATA, PDU_EVENT_ITEM, PDU_EXTRA_INFO, PDU_FLAG_DATA, PDU_INFO_DATA,
    PDU_IO_BYTEARRAY_DATA, PDU_IO_ENTITY_ADDRESS_DATA, PDU_IO_ENTITY_STATUS_DATA,
    PDU_IO_ETH_SWITCH_STATE, PDU_IO_EVENT_QUEUE_PROPERTY_DATA, PDU_IO_FILTER_DATA,
    PDU_IO_FILTER_LIST, PDU_IO_PROG_VOLTAGE_DATA, PDU_IO_TLS_CERT_CHAIN_DATA, PDU_IO_TLS_CERT_DATA,
    PDU_IO_TLS_CERTIFICATE, PDU_IO_VEHICLE_ID_REQUEST, PDU_IP_ADDR_INFO, PDU_ITEM, PDU_MODULE_DATA,
    PDU_MODULE_ITEM, PDU_PARAM_BYTEFIELD_DATA, PDU_PARAM_ITEM, PDU_PARAM_LONGFIELD_DATA,
    PDU_PARAM_STRUCT_ACCESS_TIMING, PDU_PARAM_STRUCT_SESS_TIMING,
    PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, PDU_PARAM_STRUCTFIELD_DATA, PDU_RESULT_DATA,
    PDU_RSC_CONFLICT_DATA, PDU_RSC_CONFLICT_ITEM, PDU_RSC_ID_ITEM, PDU_RSC_ID_ITEM_DATA,
    PDU_RSC_STATUS_DATA, PDU_RSC_STATUS_ITEM, PDU_STATUS_DATA, PDU_UNIQUE_RESP_ID_TABLE_ITEM,
    T_PDU_CPST, T_PDU_ERR_EVT, T_PDU_INFO, T_PDU_IT, T_PDU_PT, T_PDU_QUEUE_MODE, T_PDU_STATUS,
    UNUM32,
};

use crate::safety::{ffi_slice, ffi_slice_mut};
use crate::{DPduApiError, ModuleHandle, ParamDataType, ResourceId};

// Borrowed/owned wrappers for D-PDU item payloads.
// Type declarations live here, while most impl blocks are split into
// focused submodules by item family.

// Submodules. Each contains impl blocks for corresponding item types.
mod data;
mod data_item;
mod event;
mod module;
mod param;
mod resource;
mod unique_resp;
mod visitor;

// Re-export selected submodule types so crate consumers and sibling submodules
// (via `use super::*`) can access them without naming the submodule path.
pub use data::*;
pub use data_item::*;
pub use module::*;
pub use visitor::*;

// --- Helpers used by multiple submodules ---

fn clone_c_string(ptr: *const CHAR8) -> Option<CString> {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(ptr) }.to_owned())
    }
}

fn c_string_ptr(value: Option<&CString>) -> *mut CHAR8 {
    value
        .map(|text| text.as_ptr().cast_mut())
        .unwrap_or(std::ptr::null_mut())
}

fn vec_ptr_or_null<T>(items: &mut Vec<T>) -> *mut T {
    if items.is_empty() {
        std::ptr::null_mut()
    } else {
        items.as_mut_ptr()
    }
}

fn transparent_ref<T, U>(value: &T) -> &U {
    unsafe { &*(value as *const T as *const U) }
}

fn transparent_mut<T, U>(value: &mut T) -> &mut U {
    unsafe { &mut *(value as *mut T as *mut U) }
}

// --- Struct declarations for item types whose impls live in the submodules above ---

#[repr(transparent)]
#[derive(Debug)]
/// Borrowed wrapper for one unique-response table ECU entry.
pub struct BorrowedEcuUniqueRespData(pub PDU_ECU_UNIQUE_RESP_DATA);

#[repr(transparent)]
#[derive(Debug)]
/// Borrowed resource-status item returned by the native API.
pub struct BorrowedResourceStatusItem(pub PDU_RSC_STATUS_ITEM);

#[derive(Debug)]
/// Owned resource-status item with backing storage for FFI pointers.
pub struct OwnedResourceStatusItem {
    borrowed: BorrowedResourceStatusItem,
    resource_status_data: Vec<PDU_RSC_STATUS_DATA>,
}

#[repr(transparent)]
#[derive(Debug)]
/// Borrowed resource-id item returned by the native API.
pub struct BorrowedResourceIdItem(pub PDU_RSC_ID_ITEM);

#[derive(Debug)]
/// Owned resource-id item with stable backing allocations.
pub struct OwnedResourceIdItem {
    borrowed: BorrowedResourceIdItem,
    module_data: Vec<PDU_RSC_ID_ITEM_DATA>,
    resource_id_data: Vec<UNUM32>,
}

#[repr(transparent)]
#[derive(Debug)]
/// Borrowed resource-conflict item returned by the native API.
pub struct BorrowedResourceConflictItem(pub PDU_RSC_CONFLICT_ITEM);

#[derive(Debug)]
/// Owned resource-conflict item with backing storage.
pub struct OwnedResourceConflictItem {
    borrowed: BorrowedResourceConflictItem,
    conflict_data: Vec<PDU_RSC_CONFLICT_DATA>,
}

#[repr(transparent)]
#[derive(Debug)]
/// Borrowed COM parameter item returned by the native API.
pub struct BorrowedParamItem(pub PDU_PARAM_ITEM);

#[derive(Debug)]
/// Internal storage variants used to back encoded parameter values.
pub enum ParamStorage {
    U8(Box<u8>),
    I8(Box<i8>),
    U16(Box<u16>),
    I16(Box<i16>),
    U32(Box<u32>),
    I32(Box<i32>),
    ByteField {
        meta: Box<PDU_PARAM_BYTEFIELD_DATA>,
        data: Vec<u8>,
    },
    LongField {
        meta: Box<PDU_PARAM_LONGFIELD_DATA>,
        data: Vec<UNUM32>,
    },
    StructField {
        meta: Box<PDU_PARAM_STRUCTFIELD_DATA>,
        bytes: Vec<u8>,
    },
    Unsupported,
}

#[derive(Debug)]
/// Owned COM parameter item with associated value storage.
pub struct OwnedParamItem {
    borrowed: BorrowedParamItem,
    storage: ParamStorage,
}

#[repr(transparent)]
#[derive(Debug)]
/// Borrowed unique-response-id table item returned by the native API.
pub struct BorrowedUniqueRespIdTableItem(pub PDU_UNIQUE_RESP_ID_TABLE_ITEM);

#[derive(Debug)]
/// Owned unique-response-id table with stable backing data.
pub struct OwnedUniqueRespIdTableItem {
    borrowed: BorrowedUniqueRespIdTableItem,
    unique_data: Vec<PDU_ECU_UNIQUE_RESP_DATA>,
    param_items: Vec<PDU_PARAM_ITEM>,
    param_storages: Vec<ParamStorage>,
    scalar_u8_data: Vec<u8>,
    scalar_i8_data: Vec<i8>,
    scalar_u16_data: Vec<u16>,
    scalar_i16_data: Vec<i16>,
    scalar_u32_data: Vec<u32>,
    scalar_i32_data: Vec<i32>,
}

#[repr(transparent)]
#[derive(Debug)]
/// Borrowed event item returned by the native API.
pub struct BorrowedEventItem(pub PDU_EVENT_ITEM);

#[derive(Debug)]
/// Owned event item wrapper.
pub struct OwnedEventItem {
    borrowed: BorrowedEventItem,
}
