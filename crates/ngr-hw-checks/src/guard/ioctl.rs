//! IoCtl: which IO controls pass and which hardware controls never do here (design 13.5
//! "Guarded VCI access", fourth bullet; ADR-333 item 4).
//!
//! A control is named by its `PDU_IOCTL_*` short name (ignoring ASCII case, as the workers
//! resolve them) or by an IO control id the harness learned for such a name. The allow list is
//! the reading of the battery voltage and the housekeeping that touches only the VCI's own
//! queues, filters and buffers. Everything else is refused on every class: programming voltage,
//! pin and bus switching, repeat messages, device configuration, reset, generic and vendor
//! controls, and the other reads. A profile's `[[hardware-control]]` list does not widen this
//! yet: a control passes on a class only through a later, reviewed change.

use vci_service_interface::{IoCtlRequest, ObjectType, io_ctl_request};

use super::{Context, Grant, GuardRefusal};

/// The IOCTLs that pass on every class. `READ_VBATT` reads the battery voltage (the one VCI
/// read design 13.5 always allows); the rest empty, pause or size the VCI's transmit and receive
/// queues, the receive filters and the event queue.
const ALLOWED: &[&str] = &[
    "PDU_IOCTL_READ_VBATT",
    "PDU_IOCTL_CLEAR_TX_QUEUE",
    "PDU_IOCTL_SUSPEND_TX_QUEUE",
    "PDU_IOCTL_RESUME_TX_QUEUE",
    "PDU_IOCTL_CLEAR_RX_QUEUE",
    "PDU_IOCTL_SET_BUFFER_SIZE",
    "PDU_IOCTL_START_MSG_FILTER",
    "PDU_IOCTL_STOP_MSG_FILTER",
    "PDU_IOCTL_CLEAR_MSG_FILTER",
    "PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES",
];

pub(super) fn check(ctx: &Context<'_>, request: &IoCtlRequest) -> Result<Grant, GuardRefusal> {
    let name = match &request.io_ctrl_command {
        Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandName(name)) => name.clone(),
        Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id)) => ctx
            .learned
            .objects
            .get(&(ObjectType::ObjtIoCtrl, *id))
            .cloned()
            .ok_or(GuardRefusal::UnknownId {
                kind: "IO control",
                id: *id,
            })?,
        None => return Err(GuardRefusal::Undecodable("IoCtl without a command")),
    };
    if ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(&name)) {
        Ok(Grant::Pass)
    } else {
        Err(GuardRefusal::HardwareControl(format!(
            "{name} is not on the list of IO controls that pass"
        )))
    }
}
