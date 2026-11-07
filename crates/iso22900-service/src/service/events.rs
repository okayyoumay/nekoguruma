use std::sync::Arc;

use ngr_library_resolver::iso22900::mdf::ErrorCodes;

use tokio::sync::{Mutex, mpsc};
use tonic::Status;

use iso22900::{
    BorrowedEventItem, ComLogicalLinkHandle, DPduApi, DPduApiError, E_PDU_STATUS,
    EventNotification, EventNotificationType, ModuleHandle, PDU_HANDLE_UNDEF,
};
use vci_service_interface::{
    ComLogicalLinkHandle as VciServiceComLogicalLinkHandle,
    ComPrimitiveHandle as VciServiceComPrimitiveHandle, ErrorData, EventItem,
    EventNotification as VciServiceEventNotification, ExtraInfo, LostEventItemNotification,
    ModuleHandle as VciServiceModuleHandle, ResultData, SystemHandle,
    event_item::Data as EventItemData,
    event_notification::{EventData as EventNotificationData, Handle as EventNotificationHandle},
};

use crate::error::{map_runtime_error, map_runtime_error_for_link};
use crate::service::rpc::{CopTagMap, SubscriptionMap, UnboundItemKind};

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

/// The clock of an item's times (ADR-353): an item of a module is timed by
/// the library on that module's one time base (ISO 22900-2:2022 8.1.6.1), the
/// VCI's own clock. A system-level item belongs to no module, and the standard
/// names no time base for it, so its clock is left unspecified.
fn item_clock(h_mod: ModuleHandle) -> vci_service_interface::TimestampClock {
    if h_mod.0 == PDU_HANDLE_UNDEF {
        vci_service_interface::TimestampClock::Unspecified
    } else {
        vci_service_interface::TimestampClock::Device
    }
}

/// The kind of an item from its RxFlag: byte 3 of the four-byte flag carries the transmit
/// indication (bit 3) and start of message (bit 1) bits (ISO 22900-2 (2022) D.2.2).
fn unbound_item_kind(rx_flag: &[u8]) -> UnboundItemKind {
    match rx_flag.last().copied().unwrap_or(0) {
        flags if flags & 0x08 != 0 => UnboundItemKind::TransmitIndication,
        flags if flags & 0x02 != 0 => UnboundItemKind::StartOfMessage,
        _ => UnboundItemKind::Other,
    }
}

/// The header's identifier when it has four bytes (big-endian, as the J2534 worker and the mock
/// write it), else its length, for the warning's text.
fn header_description(header: &[u8]) -> String {
    match <[u8; 4]>::try_from(header) {
        Ok(bytes) => format!("identifier {:#x}", u32::from_be_bytes(bytes)),
        Err(_) => format!("{} bytes", header.len()),
    }
}

/// A result item of no ComPrimitive that carries extra info header bytes is outside the standard
/// (ISO 22900-2 (2022) 10.1.4.11, 10.1.4.11.4, D.2.1): design 8.11 relies on the API's own
/// tester present raising no item the agent could take for a request's (ADR-430). The item is
/// forwarded unchanged; the first one of a link and kind is warned about, the rest counted under
/// the same lock as the tags and summed up when the link or module goes (`rpc.rs`). An item of a
/// ComPrimitive (`h_cop` defined) may carry a header (10.1.4.11.4; ADR-428).
fn note_unbound_header(
    cop_tags: &mut CopTagMap,
    h_mod: ModuleHandle,
    h_cll: ComLogicalLinkHandle,
    h_cop: u32,
    rx_flag: &[u8],
    extra_info: Option<&ExtraInfo>,
) {
    let Some(extra) = extra_info else {
        return;
    };
    let header = extra.header_bytes.as_slice();
    if h_cop != PDU_HANDLE_UNDEF || header.is_empty() {
        return;
    }
    let kind = unbound_item_kind(rx_flag);
    if cop_tags.count_unbound_header(h_mod.0, h_cll.0, kind) {
        tracing::warn!(
            module = h_mod.0,
            cll = h_cll.0,
            kind = kind.label(),
            "the D-PDU library gave an item of no ComPrimitive ({}) an extra info header ({}); a \
             conforming library gives such an item no header (ADR-430); further ones on this link \
             are counted",
            kind.label(),
            header_description(header)
        );
    }
}

pub(super) fn to_proto_event_item(
    api: &DPduApi,
    borrowed: &BorrowedEventItem,
    h_mod: ModuleHandle,
    h_cll: ComLogicalLinkHandle,
    cop_tags: &mut CopTagMap,
    error_codes: &ErrorCodes,
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

            let rx_flag = result.rx_flag().bytes().map_err(map_err)?.to_vec();
            note_unbound_header(
                cop_tags,
                h_mod,
                h_cll,
                borrowed.com_primitive_handle(),
                &rx_flag,
                extra_info.as_ref(),
            );

            Some(EventItemData::ResultData(ResultData {
                rx_flag,
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
                // The item's additional error code (ISO 22900-2:2022 D.4.2) is passed on as the
                // library gives it (ADR-350), with the name the library's MDF gives it, if any
                // (ADR-351).
                Ok(error) => Some(EventItemData::ErrorData(ErrorData {
                    error_event: error.error_code_id().0 as i32,
                    extra_error_info: Some(error.extra_error_info_id()),
                    extra_error_info_name: error_codes
                        .name(error.extra_error_info_id())
                        .map(str::to_owned),
                })),
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
        timestamp_clock: item_clock(h_mod) as i32,
        data,
        cop_tag,
    })
}

pub(super) async fn run_event_subscription_task(
    mut notification_rx: mpsc::UnboundedReceiver<EventNotification>,
    api: Arc<Mutex<Option<DPduApi>>>,
    subscriptions: Arc<Mutex<SubscriptionMap>>,
    cop_tags: Arc<Mutex<CopTagMap>>,
    error_codes: Arc<ErrorCodes>,
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
                                &error_codes,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Clone, Default)]
    struct Captured(Arc<StdMutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn header(id: u32) -> ExtraInfo {
        ExtraInfo {
            header_bytes: id.to_be_bytes().to_vec(),
            footer_bytes: Vec::new(),
        }
    }

    const TX: [u8; 4] = [0, 0, 0, 0x08];
    const START: [u8; 4] = [0, 0, 0, 0x02];

    /// ADR-430: an item of no ComPrimitive with a header is warned about once per link and kind,
    /// the rest counted and summed up when the link goes; a bound item with a header (ADR-428),
    /// and an unbound one without, are not.
    #[test]
    fn an_unbound_item_with_a_header_warns_once_per_link_and_kind() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();
        let mut tags = CopTagMap::default();
        let (module, link, other_link) = (
            ModuleHandle(1),
            ComLogicalLinkHandle(2),
            ComLogicalLinkHandle(3),
        );
        tracing::subscriber::with_default(subscriber, || {
            let undef = PDU_HANDLE_UNDEF;
            for _ in 0..3 {
                note_unbound_header(&mut tags, module, link, undef, &TX, Some(&header(0x7E0)));
            }
            note_unbound_header(&mut tags, module, link, undef, &START, Some(&header(0x7E8)));
            note_unbound_header(
                &mut tags,
                module,
                other_link,
                undef,
                &TX,
                Some(&header(0x7E0)),
            );
            // Not warned about: bound with a header, unbound without one, unbound with an empty one.
            note_unbound_header(&mut tags, module, link, 7, &TX, Some(&header(0x7E0)));
            note_unbound_header(&mut tags, module, link, undef, &TX, None);
            note_unbound_header(
                &mut tags,
                module,
                link,
                undef,
                &TX,
                Some(&ExtraInfo::default()),
            );
        });
        let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let warnings = text.lines().collect::<Vec<_>>();
        assert_eq!(warnings.len(), 3, "{text}");
        assert!(warnings[0].contains("transmit indication"), "{text}");
        assert!(warnings[0].contains("identifier 0x7e0"), "{text}");
        assert!(warnings[0].contains("cll=2"), "{text}");
        assert!(warnings[0].contains("ADR-430"), "{text}");
        assert!(warnings[1].contains("start of message"), "{text}");
        assert!(warnings[2].contains("cll=3"), "{text}");
        // The link's destruction gives the one count above one; the other kinds had one item.
        let counts = tags.take_unbound_header_counts(|m, c| m == module.0 && c == link.0);
        assert_eq!(counts, [(1, 2, UnboundItemKind::TransmitIndication, 3)]);
        assert!(
            tags.take_unbound_header_counts(|_, c| c == link.0)
                .is_empty()
        );
    }

    #[test]
    fn the_kind_comes_from_the_rx_flag_and_a_header_not_of_four_bytes_gives_its_length() {
        assert_eq!(unbound_item_kind(&TX), UnboundItemKind::TransmitIndication);
        assert_eq!(unbound_item_kind(&START), UnboundItemKind::StartOfMessage);
        assert_eq!(unbound_item_kind(&[0; 4]), UnboundItemKind::Other);
        assert_eq!(unbound_item_kind(&[]), UnboundItemKind::Other);
        assert_eq!(header_description(&[0, 0, 7, 0xE0]), "identifier 0x7e0");
        assert_eq!(header_description(&[1, 2]), "2 bytes");
    }

    /// ADR-353: a module's items are on the VCI's clock; a system-level item,
    /// which belongs to no module, names none.
    #[test]
    fn an_item_of_a_module_is_on_the_device_clock_and_a_system_item_on_none() {
        assert_eq!(
            item_clock(ModuleHandle(1)),
            vci_service_interface::TimestampClock::Device
        );
        assert_eq!(
            item_clock(ModuleHandle(PDU_HANDLE_UNDEF)),
            vci_service_interface::TimestampClock::Unspecified
        );
    }
}
