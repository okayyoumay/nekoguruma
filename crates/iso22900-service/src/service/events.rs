use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tonic::Status;

use iso22900::{
    BorrowedEventItem, ComLogicalLinkHandle, DPduApi, DPduApiError, E_PDU_STATUS,
    EventNotification, EventNotificationType, ModuleHandle, PDU_HANDLE_UNDEF,
};
use vci_service_interface::{
    ComLogicalLinkHandle as VciServiceComLogicalLinkHandle,
    ComPrimitiveHandle as VciServiceComPrimitiveHandle, EventItem,
    EventNotification as VciServiceEventNotification, ExtraInfo, LostEventItemNotification,
    ModuleHandle as VciServiceModuleHandle, ResultData, SystemHandle,
    event_item::Data as EventItemData,
    event_notification::{EventData as EventNotificationData, Handle as EventNotificationHandle},
};

use crate::error::{map_runtime_error, map_runtime_error_for_link};
use crate::service::rpc::{CopTagMap, SubscriptionMap};

const PDU_ERR_EVENT_QUEUE_EMPTY: u32 = 113;

/// Routes a native conversion/call failure through the rich `ErrorDetail`
/// mapper (ADR-105): the CLL-scoped variant when `module`/`cll` are real
/// handles, the plain variant otherwise (System/Module-scoped
/// notifications carry `PDU_HANDLE_UNDEF` for one or both).
fn rich_runtime_error(
    api: &DPduApi,
    module: ModuleHandle,
    cll: ComLogicalLinkHandle,
    error: DPduApiError,
) -> Status {
    if module.0 != PDU_HANDLE_UNDEF && cll.0 != PDU_HANDLE_UNDEF {
        map_runtime_error_for_link(api, module, cll, error)
    } else {
        map_runtime_error(error)
    }
}

fn to_notification_handle(
    module_handle: ModuleHandle,
    cll_handle: ComLogicalLinkHandle,
) -> EventNotificationHandle {
    if module_handle.0 != PDU_HANDLE_UNDEF && cll_handle.0 != PDU_HANDLE_UNDEF {
        EventNotificationHandle::CllHandle(VciServiceComLogicalLinkHandle {
            module_handle: module_handle.0,
            cll_handle: cll_handle.0,
        })
    } else if module_handle.0 != PDU_HANDLE_UNDEF && cll_handle.0 == PDU_HANDLE_UNDEF {
        EventNotificationHandle::ModuleHandle(VciServiceModuleHandle {
            module_handle: module_handle.0,
        })
    } else {
        EventNotificationHandle::SystemHandle(SystemHandle {})
    }
}

pub(super) fn to_proto_event_item(
    api: &DPduApi,
    borrowed: &BorrowedEventItem,
    h_mod: ModuleHandle,
    h_cll: ComLogicalLinkHandle,
    cop_tags: &mut CopTagMap,
) -> Result<EventItem, Status> {
    let map_err = |error: DPduApiError| rich_runtime_error(api, h_mod, h_cll, error);

    let data = match borrowed.result_data() {
        Ok(result) => {
            let extra_info = match result.extra_info() {
                Some(extra) => Some(ExtraInfo {
                    header_bytes: extra.header_bytes().map_err(map_err)?.to_vec(),
                    footer_bytes: extra.footer_bytes().map_err(map_err)?.to_vec(),
                }),
                None => None,
            };

            let (has_tx_msg_done_timestamp, has_start_msg_timestamp) = result
                .timestamp_flags()
                .bytes()
                .map_err(map_err)?
                .first()
                .map_or((false, false), |flags| {
                    ((flags & 0x80) != 0, (flags & 0x40) != 0)
                });

            Some(EventItemData::ResultData(ResultData {
                rx_flag: result.rx_flag().bytes().map_err(map_err)?.to_vec(),
                unique_resp_identifier: result.unique_resp_identifier(),
                acceptance_id: result.acceptance_id(),
                tx_msg_done_timestamp: if has_tx_msg_done_timestamp {
                    Some(result.tx_msg_done_timestamp())
                } else {
                    None
                },
                start_msg_timestamp: if has_start_msg_timestamp {
                    Some(result.start_msg_timestamp())
                } else {
                    None
                },
                extra_info,
                data_bytes: result.data_bytes().map_err(map_err)?.to_vec(),
            }))
        }
        Err(DPduApiError::Unsupported(_)) => match borrowed.status_data() {
            Ok(status) => Some(EventItemData::CopStatus(status.status() as i32)),
            Err(DPduApiError::Unsupported(_)) => match borrowed.error_data() {
                Ok(error) => Some(EventItemData::ErrorData(error.error_code_id().0 as i32)),
                Err(DPduApiError::Unsupported(_)) => match borrowed.info_data() {
                    Ok(info) => Some(EventItemData::InfoData(info.info_code().0 as i32)),
                    Err(DPduApiError::Unsupported(_)) => None,
                    Err(error) => return Err(map_err(error)),
                },
                Err(error) => return Err(map_err(error)),
            },
            Err(error) => return Err(map_err(error)),
        },
        Err(error) => return Err(map_err(error)),
    };

    let cop_handle = if borrowed.com_primitive_handle() == PDU_HANDLE_UNDEF {
        None
    } else {
        Some(VciServiceComPrimitiveHandle {
            module_handle: h_mod.0,
            cll_handle: h_cll.0,
            cop_handle: borrowed.com_primitive_handle(),
        })
    };

    // ADR-204: echo the stored tag verbatim (never remove it here -- a COP
    // may emit many events before it terminates), then evict it below, after
    // the echo, the moment this event's own cop_status reaches a terminal
    // value. Per ISO 22900-2 (D-PDU API destroys a COP internally once
    // PDU_COPST_FINISHED/PDU_COPST_CANCELLED is reached; no further events
    // are ever queued for it afterward), so waiting for the real terminal
    // event -- not `CancelComPrimitive` itself, which is asynchronous -- is
    // the correct eviction point (mirrors j2534-0404-service's
    // cancelled_cops/primitives pattern, ADR-002/ADR-021).
    let cop_tag = cop_handle.as_ref().and_then(|handle| {
        cop_tags
            .get(&(handle.module_handle, handle.cll_handle, handle.cop_handle))
            .cloned()
    });

    if let (Some(handle), Some(EventItemData::CopStatus(status))) = (cop_handle.as_ref(), &data) {
        // `E_PDU_STATUS`'s raw field is bindgen-generated per target and its
        // underlying integer type is not consistent across this crate's
        // committed target bindings (`i32` on x86_64-pc-windows-msvc, `u32`
        // on every other target). Widen everything to `i64` for the
        // comparison: `i64` is never the enum's own generated type on any
        // committed target, so the cast is a genuine widening conversion
        // everywhere -- unlike casting to `u32`/`i32` directly, which is a
        // real cast on some targets but a clippy `unnecessary_cast` no-op on
        // whichever target happens to already use that same type natively.
        let status = i64::from(*status);
        let is_terminal = status == i64::from(E_PDU_STATUS::PDU_COPST_FINISHED.0)
            || status == i64::from(E_PDU_STATUS::PDU_COPST_CANCELLED.0);
        if is_terminal {
            cop_tags.remove(&(handle.module_handle, handle.cll_handle, handle.cop_handle));
        }
    }

    Ok(EventItem {
        cop_handle,
        timestamp: borrowed.timestamp(),
        data,
        cop_tag,
    })
}

pub(super) async fn run_event_subscription_task(
    mut notification_rx: mpsc::UnboundedReceiver<EventNotification>,
    api: Arc<Mutex<Option<DPduApi>>>,
    subscriptions: Arc<Mutex<SubscriptionMap>>,
    cop_tags: Arc<Mutex<CopTagMap>>,
) {
    while let Some(notification) = notification_rx.recv().await {
        let handle =
            to_notification_handle(notification.module_handle, notification.logical_link_handle);
        let key = (
            notification.module_handle.0,
            notification.logical_link_handle.0,
        );
        let stream_tx = {
            let subscriptions = subscriptions.lock().await;
            subscriptions.get(&key).cloned()
        };

        let Some(stream_tx) = stream_tx else {
            continue;
        };

        if notification.event_type == EventNotificationType::DataLost {
            let _ = stream_tx.send(Ok(VciServiceEventNotification {
                handle: Some(handle),
                event_data: Some(EventNotificationData::Lost(LostEventItemNotification {})),
            }));
            continue;
        }

        if notification.event_type != EventNotificationType::DataAvailable {
            continue;
        }

        loop {
            let next_outbound = {
                let api_guard = api.lock().await;
                let mut cop_tags_guard = cop_tags.lock().await;
                match api_guard.as_ref() {
                    None => Some(Err(Status::failed_precondition("API not initialized"))),
                    Some(api_ref) => match api_ref.get_event_item(
                        notification.module_handle,
                        notification.logical_link_handle,
                    ) {
                        Ok(event_item) => Some(
                            to_proto_event_item(
                                api_ref,
                                event_item.borrowed(),
                                notification.module_handle,
                                notification.logical_link_handle,
                                &mut cop_tags_guard,
                            )
                            .map(|item| VciServiceEventNotification {
                                handle: Some(handle),
                                event_data: Some(EventNotificationData::Item(item)),
                            }),
                        ),
                        Err(DPduApiError::PduError(code)) if code == PDU_ERR_EVENT_QUEUE_EMPTY => {
                            None
                        }
                        Err(error) => Some(Err(rich_runtime_error(
                            api_ref,
                            notification.module_handle,
                            notification.logical_link_handle,
                            error,
                        ))),
                    },
                }
            };

            match next_outbound {
                Some(outbound) => {
                    if stream_tx.send(outbound).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    }
}
