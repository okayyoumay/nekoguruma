use tonic::Status;

use iso22900::{
    ComLogicalLinkHandle, ComPrimitiveHandle, ModuleHandle, PDU_HANDLE_UNDEF, ResourceId,
};
use vci_service_interface::{
    get_event_item_request::Handle as GetEventItemHandle,
    get_status_request::Handle as GetStatusHandle, io_ctl_request::Handle as IoCtlHandle,
    status_response::Status as VciServiceStatusResponse,
    subscribe_event_request::Handle as SubscribeEventHandle,
};

const REQUIRED_HANDLE_MESSAGE: &str = "handle is required";

pub(super) fn module_handle(value: u32) -> ModuleHandle {
    ModuleHandle(value)
}

pub(super) fn logical_link_handle(value: u32) -> ComLogicalLinkHandle {
    ComLogicalLinkHandle(value)
}

pub(super) fn primitive_handle(value: u32) -> ComPrimitiveHandle {
    ComPrimitiveHandle(value)
}

pub(super) fn resource_id(value: u32) -> ResourceId {
    ResourceId(value)
}

pub(super) fn require_module_message(
    handle: Option<vci_service_interface::ModuleHandle>,
    field_name: &str,
) -> Result<ModuleHandle, Status> {
    let handle =
        handle.ok_or_else(|| Status::invalid_argument(format!("{field_name} is required")))?;
    Ok(module_handle(handle.module_handle))
}

pub(super) fn require_cll_message(
    handle: Option<vci_service_interface::ComLogicalLinkHandle>,
    field_name: &str,
) -> Result<(ModuleHandle, ComLogicalLinkHandle), Status> {
    let handle =
        handle.ok_or_else(|| Status::invalid_argument(format!("{field_name} is required")))?;
    Ok((
        module_handle(handle.module_handle),
        logical_link_handle(handle.cll_handle),
    ))
}

pub(super) fn require_cop_message(
    handle: Option<vci_service_interface::ComPrimitiveHandle>,
    field_name: &str,
) -> Result<(ModuleHandle, ComLogicalLinkHandle, ComPrimitiveHandle), Status> {
    let handle =
        handle.ok_or_else(|| Status::invalid_argument(format!("{field_name} is required")))?;
    Ok((
        module_handle(handle.module_handle),
        logical_link_handle(handle.cll_handle),
        primitive_handle(handle.cop_handle),
    ))
}

pub(super) fn to_proto_cll_handle(
    module: ModuleHandle,
    cll: ComLogicalLinkHandle,
) -> vci_service_interface::ComLogicalLinkHandle {
    vci_service_interface::ComLogicalLinkHandle {
        module_handle: module.0,
        cll_handle: cll.0,
    }
}

pub(super) fn to_proto_cop_handle(
    module: ModuleHandle,
    cll: ComLogicalLinkHandle,
    cop: ComPrimitiveHandle,
) -> vci_service_interface::ComPrimitiveHandle {
    vci_service_interface::ComPrimitiveHandle {
        module_handle: module.0,
        cll_handle: cll.0,
        cop_handle: cop.0,
    }
}

/// Normalized form of a handle field that is present in multiple request types.
/// All three request types (GetEventItem, SubscribeEvent, IoCtl) share the
/// same three-variant structure, so we convert them to this common form.
enum ModuleCllHandle {
    System,
    Module(u32),
    Cll(u32, u32),
}

impl From<GetEventItemHandle> for ModuleCllHandle {
    fn from(h: GetEventItemHandle) -> Self {
        match h {
            GetEventItemHandle::SystemHandle(_) => Self::System,
            GetEventItemHandle::ModuleHandle(h) => Self::Module(h.module_handle),
            GetEventItemHandle::CllHandle(h) => Self::Cll(h.module_handle, h.cll_handle),
        }
    }
}

impl From<SubscribeEventHandle> for ModuleCllHandle {
    fn from(h: SubscribeEventHandle) -> Self {
        match h {
            SubscribeEventHandle::SystemHandle(_) => Self::System,
            SubscribeEventHandle::ModuleHandle(h) => Self::Module(h.module_handle),
            SubscribeEventHandle::CllHandle(h) => Self::Cll(h.module_handle, h.cll_handle),
        }
    }
}

impl From<IoCtlHandle> for ModuleCllHandle {
    fn from(h: IoCtlHandle) -> Self {
        match h {
            IoCtlHandle::SystemHandle(_) => Self::System,
            IoCtlHandle::ModuleHandle(h) => Self::Module(h.module_handle),
            IoCtlHandle::CllHandle(h) => Self::Cll(h.module_handle, h.cll_handle),
        }
    }
}

fn parse_module_cll<H: Into<ModuleCllHandle>>(
    handle: Option<H>,
) -> Result<(ModuleHandle, ComLogicalLinkHandle), Status> {
    match handle.map(Into::into) {
        Some(ModuleCllHandle::System) => Ok((
            module_handle(PDU_HANDLE_UNDEF),
            logical_link_handle(PDU_HANDLE_UNDEF),
        )),
        Some(ModuleCllHandle::Module(m)) => {
            Ok((module_handle(m), logical_link_handle(PDU_HANDLE_UNDEF)))
        }
        Some(ModuleCllHandle::Cll(m, cll)) => Ok((module_handle(m), logical_link_handle(cll))),
        None => Err(Status::invalid_argument(REQUIRED_HANDLE_MESSAGE)),
    }
}

pub(super) fn parse_event_item_handle(
    handle: Option<GetEventItemHandle>,
) -> Result<(ModuleHandle, ComLogicalLinkHandle), Status> {
    parse_module_cll(handle)
}

pub(super) fn parse_subscribe_event_handle(
    handle: Option<SubscribeEventHandle>,
) -> Result<(ModuleHandle, ComLogicalLinkHandle), Status> {
    parse_module_cll(handle)
}

pub(super) fn parse_ioctl_handle(
    handle: Option<IoCtlHandle>,
) -> Result<(ModuleHandle, ComLogicalLinkHandle), Status> {
    parse_module_cll(handle)
}

pub(super) fn parse_status_handle(
    handle: Option<GetStatusHandle>,
) -> Result<
    (
        ModuleHandle,
        ComLogicalLinkHandle,
        ComPrimitiveHandle,
        VciServiceStatusResponse,
    ),
    Status,
> {
    match handle {
        Some(GetStatusHandle::ModuleHandle(handle)) => Ok((
            module_handle(handle.module_handle),
            logical_link_handle(PDU_HANDLE_UNDEF),
            primitive_handle(PDU_HANDLE_UNDEF),
            VciServiceStatusResponse::ModuleStatus(0),
        )),
        Some(GetStatusHandle::CllHandle(handle)) => Ok((
            module_handle(handle.module_handle),
            logical_link_handle(handle.cll_handle),
            primitive_handle(PDU_HANDLE_UNDEF),
            VciServiceStatusResponse::CllStatus(0),
        )),
        Some(GetStatusHandle::CopHandle(handle)) => Ok((
            module_handle(handle.module_handle),
            logical_link_handle(handle.cll_handle),
            primitive_handle(handle.cop_handle),
            VciServiceStatusResponse::CopStatus(0),
        )),
        None => Err(Status::invalid_argument(REQUIRED_HANDLE_MESSAGE)),
    }
}
