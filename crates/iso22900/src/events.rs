use std::{collections::HashMap, ffi::c_void, sync::Mutex};

use iso22900_sys::bindings::{T_PDU_EVT_DATA, UNUM32};

use crate::{ComLogicalLinkHandle, EventNotification, EventNotificationType, ModuleHandle};

type EventCallback = Box<dyn FnMut(EventNotification) + Send + 'static>;

pub(crate) struct ApiTagContext {
    pub(crate) callbacks: Mutex<HashMap<(ModuleHandle, ComLogicalLinkHandle), EventCallback>>,
}

impl ApiTagContext {
    pub(crate) fn new() -> Self {
        Self {
            callbacks: Mutex::new(HashMap::new()),
        }
    }
}

unsafe fn dispatch_event_callback(
    event_type: T_PDU_EVT_DATA,
    h_mod: UNUM32,
    h_cll: UNUM32,
    // Retained in the signature to mirror the native callback's own
    // parameter list (this fn is the shared body both calling-convention
    // trampolines below delegate to), but the value itself is unused: no
    // downstream reader exists for a CLL-scoped tag (ADR-204 removed the
    // gRPC-facing `cll_tag` field after confirming this).
    _p_cll_tag: *mut c_void,
    p_api_tag: *mut c_void,
) {
    if p_api_tag.is_null() {
        return;
    }

    let context = unsafe { &*(p_api_tag as *const ApiTagContext) };

    let notification = EventNotification {
        event_type: EventNotificationType::from_raw(event_type),
        module_handle: ModuleHandle(h_mod),
        logical_link_handle: ComLogicalLinkHandle(h_cll),
    };

    if let Ok(mut callbacks) = context.callbacks.lock()
        && let Some(callback) =
            callbacks.get_mut(&(ModuleHandle(h_mod), ComLogicalLinkHandle(h_cll)))
    {
        callback(notification);
    }
}

#[cfg(all(windows, target_arch = "x86"))]
pub(crate) unsafe extern "stdcall" fn event_callback_trampoline(
    event_type: T_PDU_EVT_DATA,
    h_mod: UNUM32,
    h_cll: UNUM32,
    p_cll_tag: *mut c_void,
    p_api_tag: *mut c_void,
) {
    unsafe { dispatch_event_callback(event_type, h_mod, h_cll, p_cll_tag, p_api_tag) }
}

#[cfg(not(all(windows, target_arch = "x86")))]
pub(crate) unsafe extern "C" fn event_callback_trampoline(
    event_type: T_PDU_EVT_DATA,
    h_mod: UNUM32,
    h_cll: UNUM32,
    p_cll_tag: *mut c_void,
    p_api_tag: *mut c_void,
) {
    unsafe { dispatch_event_callback(event_type, h_mod, h_cll, p_cll_tag, p_api_tag) }
}
