//! Shared gRPC client scaffolding for the `j2534-0404-service` protocol
//! examples in this directory (`grpc_can.rs`, `grpc_iso15765.rs`,
//! `grpc_iso9141.rs`, `grpc_iso14230.rs`, `grpc_j1850vpw.rs`,
//! `grpc_j1850pwm.rs`, `grpc_sci.rs`, `grpc_uart_echo_byte.rs`,
//! `grpc_honda_diagh.rs`, `grpc_j1708.rs`, `grpc_j1939.rs`, `grpc_tp2_0.rs`,
//! `grpc_gm_uart.rs`, and `grpc_ethernet_ndis.rs` -- one file per J2534
//! protocol family, covering all 14 non-`ANALOG_IN` families this codebase
//! implements).
//!
//! Every example here is a **pure gRPC client** connecting to an
//! already-running `j2534-0404-service` process started separately (see
//! `docs/startup-spec.md`, e.g. `cargo run -p j2534-0404-service --
//! j2534-0404:<library-name>?port=<PORT>`) -- this is deliberately
//! different from `j2534-0404-service/tests/live_grpc_flow.rs`, which
//! embeds the server in-process as a test harness. Nothing in this module
//! starts a server.
//!
//! This module holds only transport/lifecycle boilerplate that every
//! protocol example needs verbatim: connecting `VciServiceClient`,
//! `GetModuleIds` -> `ModuleConnect` -> `GetVersion`, `SubscribeEvent`
//! setup, teardown (`DisconnectComLogicalLink` -> `DestroyComLogicalLink`
//! -> `ModuleDisconnect`), CLI-arg parsing for the mandatory service
//! address, and small generic utilities (`GetObjectId` shortname
//! resolution, hex/byte-list argument parsing, event-stream waiting).
//! Protocol-specific logic -- resource-row selection (`GetResourceIds`),
//! `SetComParam`/`SetUniqueRespIdTable` addressing, and the actual
//! `CoptSendrecv` payload -- stays in each protocol's own example file and
//! is NOT provided here.
//!
//! ## Adding a new protocol example
//!
//! 1. Create `examples/grpc_<protocol>.rs` and add
//!    `#[path = "common/mod.rs"] mod common;` near the top (Cargo's
//!    default single-file example layout means an example can't `mod` a
//!    sibling directory implicitly -- this explicit `#[path]` is required
//!    and has been verified to work with `cargo check --examples`).
//! 2. In `main`, parse args with `common::parse_common_args` to get the
//!    mandatory service address, then parse any protocol-specific
//!    positional args afterward (pins, addressing overrides, payload
//!    bytes) using `common::parse_u32_arg`/`common::parse_data_bytes_arg`.
//!    Wrap the async body in a `run()` function returning
//!    `common::Result<()>` and use `?` throughout; `main` calls
//!    `run().await` and on `Err` prints it and exits with status 1 (see
//!    any existing `grpc_*.rs` file for the exact shape).
//! 3. `common::connect_and_open_module(&args.service_addr).await?` to get
//!    a connected `VciServiceClient` plus a `ModuleHandle` already past
//!    `GetModuleIds`/`ModuleConnect`/`GetVersion` (version info printed).
//! 4. Resolve every ComParam shortname you need via
//!    `common::comparam_object_id(&mut client, "CP_...").await?`.
//! 5. Call `client.get_resource_ids(...)` /
//!    `client.create_com_logical_link(...)` directly -- protocol-specific
//!    resource row, NOT provided by this module.
//! 6. Call `client.set_com_param(...)` / `client.set_unique_resp_id_table(...)`
//!    directly -- protocol-specific addressing, NOT provided by this
//!    module. `SetUniqueRespIdTable`'s CAN-specific entries are
//!    `PDU_PC_UNIQUE_ID`-class only for the CAN protocol family (ADR-042),
//!    but KWP (ISO9141/ISO14230) and J1850 (VPW/PWM) examples still call
//!    `SetUniqueRespIdTable` with a `CP_EcuRespSourceAddress`-keyed entry
//!    (ADR-202/ADR-203) to constrain `CoptSendrecv` responses to the
//!    physically addressed ECU on a shared bus.
//! 7. `client.connect_com_logical_link(...)`, then
//!    `common::subscribe(&mut client, cll_handle).await?`.
//! 8. Drive `StartComPrimitive` (`CoptStartcomm` -> `CoptSendrecv` ->
//!    `CoptStopcomm`) directly against `client`, using
//!    `common::wait_for_event`/`common::wait_for_send_recv_response` to
//!    watch the event stream. Every `StartComPrimitiveRequest` must set a
//!    distinct, non-empty `cop_tag` (e.g. `b"startcomm"`, `b"sendrecv"`,
//!    `b"stopcomm"`), and the SAME bytes must be passed to the matching
//!    `common::wait_for_cop_finished`/`common::wait_for_send_recv_response`
//!    call for that COP (ADR-204) -- without this, a stale terminal event
//!    left over from an earlier, already-timed-out primitive on the same CLL
//!    can be mistaken for the current call's own completion.
//! 9. `common::teardown(&mut client, cll_handle, module_handle).await?`.
//!
//! ## Accepted limitation: no teardown on early error
//!
//! Every `run()` in this directory propagates errors with `?` and has no
//! `Drop`/`catch`-style guard around `common::teardown`, so a rejected
//! call partway through the flow (`SetComParam`, `ConnectComLogicalLink`,
//! `StartComPrimitive`, ...) skips `DisconnectComLogicalLink` /
//! `DestroyComLogicalLink` / `ModuleDisconnect` entirely and the process
//! exits with the CLL/module handle still allocated server-side. This is
//! a deliberate simplicity tradeoff for demo binaries (adding
//! `Drop`-based unwind-safe cleanup would obscure the very call sequence
//! these examples exist to demonstrate) rather than an oversight, but it
//! does mean an interrupted example run against real hardware may require
//! restarting `j2534-0404-service` to release the stale handle before
//! retrying.

#![allow(dead_code)] // not every example in this directory uses every helper

use std::time::Duration;

use tonic::Streaming;
use tonic::transport::Channel;
use vci_service_interface::{
    ComLogicalLinkHandle, DestroyComLogicalLinkRequest, DisconnectComLogicalLinkRequest, EventItem,
    EventNotification, GetModuleIdsRequest, GetObjectIdRequest, GetVersionRequest,
    ModuleConnectRequest, ModuleDisconnectRequest, ModuleHandle, ObjectType, SubscribeEventRequest,
    event_item, event_notification, subscribe_event_request, vci_service_client::VciServiceClient,
};

/// A boxed error type broad enough for gRPC transport errors
/// (`tonic::transport::Error`), gRPC call errors (`tonic::Status`), and
/// this module's own argument-parsing errors -- every example's `run()`
/// returns this via `?` and `main` prints it and exits(1).
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, BoxError>;

/// Default response-wait window for `wait_for_event`/
/// `wait_for_send_recv_response` when a protocol example doesn't override
/// it. Matches `live_grpc_flow.rs`'s own `J2534_LIVE_RESPONSE_TIMEOUT_MS`
/// default.
pub const DEFAULT_RESPONSE_TIMEOUT_MS: u32 = 2000;

/// The one CLI argument every example shares: the running service's gRPC
/// address (e.g. `http://127.0.0.1:60124`, the port the
/// `?port=` startup argument bound, or whatever the service printed on
/// startup if `?port=` was omitted -- there is no fixed default port).
pub struct CommonArgs {
    pub service_addr: String,
}

/// Parses the mandatory first positional argument (service address) off
/// `args`, leaving the iterator positioned at the first protocol-specific
/// argument. Exits the process with status 2 and a usage message (matching
/// this workspace's other example binaries, e.g.
/// `j2534-0404/examples/read_version_0404.rs`) if it's missing.
///
/// `bin` is the program name (`args.next()`'s own first value); `extra_usage`
/// describes the protocol-specific arguments that follow, e.g.
/// `"[phys-req-id] [resp-id] [baud-rate]"`.
pub fn parse_common_args(bin: &str, args: &mut std::env::Args, extra_usage: &str) -> CommonArgs {
    match args.next() {
        Some(service_addr) => CommonArgs { service_addr },
        None => {
            usage(bin, extra_usage);
            std::process::exit(2);
        }
    }
}

/// Prints a `Usage:` line combining the common `<service-addr>` argument
/// with a protocol-specific `extra_usage` fragment.
pub fn usage(bin: &str, extra_usage: &str) {
    eprintln!("Usage: {bin} <service-addr> {extra_usage}");
    eprintln!("Example: {bin} http://127.0.0.1:60124");
}

/// Parses a decimal or `0x`-prefixed hex `u32` CLI argument, exiting the
/// process with status 2 and a usage message on failure (mirrors
/// `j2534-0404/examples/channel_read_write_0404.rs`'s `parse_u32_arg`).
pub fn parse_u32_arg(bin: &str, extra_usage: &str, name: &str, raw: &str) -> u32 {
    let value = raw.trim();
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => value.parse::<u32>(),
    };
    match parsed {
        Ok(v) => v,
        Err(err) => {
            eprintln!("invalid {name} '{raw}': {err}");
            usage(bin, extra_usage);
            std::process::exit(2);
        }
    }
}

/// Parses a comma-separated list of hex bytes (each optionally
/// `0x`-prefixed, e.g. `"22,F1,90"`) into a `Vec<u8>` -- the
/// `CoptSendrecv` payload format every protocol example accepts as an
/// override. Exits the process with status 2 and a usage message on
/// failure.
pub fn parse_data_bytes_arg(bin: &str, extra_usage: &str, name: &str, raw: &str) -> Vec<u8> {
    raw.split(',')
        .map(|part| {
            let token = part.trim();
            let token = token
                .strip_prefix("0x")
                .or_else(|| token.strip_prefix("0X"))
                .unwrap_or(token);
            u8::from_str_radix(token, 16).unwrap_or_else(|err| {
                eprintln!("invalid byte '{part}' in {name} '{raw}': {err}");
                usage(bin, extra_usage);
                std::process::exit(2);
            })
        })
        .collect()
}

/// Connects to `service_addr`, then drives `GetModuleIds` -> `ModuleConnect`
/// -> `GetVersion` (printing the reported version info) -- the boilerplate
/// every protocol example needs before it can call `GetResourceIds`.
/// Returns the connected client plus the module handle it just connected.
pub async fn connect_and_open_module(
    service_addr: &str,
) -> Result<(VciServiceClient<Channel>, ModuleHandle)> {
    let mut client = VciServiceClient::connect(service_addr.to_string()).await?;

    let module_ids = client
        .get_module_ids(GetModuleIdsRequest {})
        .await?
        .into_inner();
    let module_handle: ModuleHandle = module_ids
        .module_id_list
        .and_then(|list| list.module_data.into_iter().next())
        .and_then(|data| data.module_handle)
        .ok_or("no module reported by GetModuleIds -- is the service configured with a device?")?;
    println!("module_handle = {}", module_handle.module_handle);

    client
        .module_connect(ModuleConnectRequest {
            module_handle: Some(module_handle),
        })
        .await?;

    let version = client
        .get_version(GetVersionRequest {
            module_handle: Some(module_handle),
        })
        .await?
        .into_inner();
    if let Some(v) = version.version_data {
        println!(
            "device: firmware={:?} dll={:?} api_sw={:?} (parsed api_sw_version={})",
            v.hw_name, v.fw_name, v.pdu_api_sw_name, v.pdu_api_sw_version
        );
    }

    Ok((client, module_handle))
}

/// Resolves a ComParam shortname (e.g. `"CP_Baudrate"`) to its
/// `pdu_object_id` via `GetObjectId`, as a real D-PDU client would instead
/// of hardcoding MDF ids.
pub async fn comparam_object_id(
    client: &mut VciServiceClient<Channel>,
    shortname: &str,
) -> Result<u32> {
    let resp = client
        .get_object_id(GetObjectIdRequest {
            object_type: ObjectType::ObjtComparam as i32,
            shortname: shortname.to_string(),
        })
        .await
        .map_err(|e| format!("get_object_id({shortname}) failed: {e}"))?
        .into_inner();
    println!("{shortname} object_id = {:#x}", resp.pdu_object_id);
    Ok(resp.pdu_object_id)
}

/// Subscribes to the event stream for `cll_handle` -- call this after
/// `ConnectComLogicalLink` succeeds, before issuing any `StartComPrimitive`
/// call whose progress you want to observe.
pub async fn subscribe(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
) -> Result<Streaming<EventNotification>> {
    let events = client
        .subscribe_event(SubscribeEventRequest {
            handle: Some(subscribe_event_request::Handle::CllHandle(cll_handle)),
        })
        .await?
        .into_inner();
    Ok(events)
}

/// Prints a single `EventItem` in a human-readable form -- used by
/// `wait_for_event` so every example gets full visibility into the event
/// sequence, not just the one item its predicate is waiting for.
pub fn print_event_item(item: &EventItem) {
    match &item.data {
        Some(event_item::Data::ResultData(result)) => println!(
            "  event: ResultData data={:02x?} unique_resp_id={}",
            result.data_bytes, result.unique_resp_identifier
        ),
        Some(event_item::Data::CllStatus(status)) => println!("  event: CllStatus({status})"),
        Some(event_item::Data::CopStatus(status)) => println!("  event: CopStatus({status})"),
        Some(event_item::Data::ErrorData(err)) => println!("  event: ErrorData({err})"),
        Some(event_item::Data::ModuleStatus(status)) => println!("  event: ModuleStatus({status})"),
        Some(event_item::Data::InfoData(info)) => println!("  event: InfoData({info})"),
        None => println!("  event: (empty)"),
    }
}

/// Polls `events` until `predicate` returns `true` for an `EventItem`, or
/// `timeout_ms` elapses (returns `false` on timeout, stream end, or a
/// stream error). Every item seen along the way is printed.
pub async fn wait_for_event<F>(
    events: &mut Streaming<EventNotification>,
    timeout_ms: u32,
    mut predicate: F,
) -> bool
where
    F: FnMut(&EventItem) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let Ok(poll_result) = tokio::time::timeout(remaining, events.message()).await else {
            return false; // timed out waiting for the next message
        };
        let Ok(Some(notification)) = poll_result else {
            return false; // stream ended or errored
        };
        if let Some(event_notification::EventData::Item(item)) = notification.event_data {
            print_event_item(&item);
            if predicate(&item) {
                return true;
            }
        }
    }
}

/// Drives a `CoptSendrecv` to completion via the event stream: collects the
/// last `ResultData` seen before the terminal `CopStatus` (Finished or
/// Cancelled), or `None` if nothing arrived before timeout.
///
/// `cop_tag` must be the exact bytes passed as `StartComPrimitiveRequest.cop_tag`
/// for the `CoptSendrecv` call this wait is for (ADR-204). An event belonging to
/// a different, still-outstanding COP -- most commonly a stale event from an
/// earlier primitive whose own wait already timed out -- carries either no
/// `cop_tag` or a different one and is ignored (the predicate returns `false`
/// and the loop keeps waiting), never mistaken for this call's own terminal
/// event.
///
/// Unlike [`wait_for_cop_finished`], this function's return value is never
/// used by any caller in this directory to gate a subsequent primitive --
/// every call site already treats `None` conservatively (a printed WARNING,
/// then it proceeds to `CoptStopcomm` regardless), since `None` is also the
/// ordinary "no ECU answered" outcome. A `CoptSendrecv` TX failure (e.g. a
/// software-ISO-TP size rejection, a failed RC21/RC23 re-request --
/// `events.rs`'s `handle_send_recv`) is likewise surfaced as a tagged
/// `ErrorData` event followed by the same COP's `PduCopstFinished`, and
/// since `ErrorData` carries no `ResultData`, the existing `CopStatus`-only
/// predicate already correctly ends the wait with `response == None` -- so
/// there is no false-success gating bug here the way there was in
/// `wait_for_cop_finished`. What WAS missing: the resulting `None` prints
/// the same generic "no response" WARNING regardless of cause, silently
/// misattributing a real TX/comms error to "no ECU answered". This function
/// now tracks a matching `ErrorData` the same way `wait_for_cop_finished`
/// does and prints it, purely as a diagnostic improvement -- it does not
/// change the `Option<Vec<u8>>` return value or any caller's control flow.
pub async fn wait_for_send_recv_response(
    events: &mut Streaming<EventNotification>,
    timeout_ms: u32,
    cop_tag: &[u8],
) -> Option<Vec<u8>> {
    let mut response: Option<Vec<u8>> = None;
    let mut saw_error: Option<i32> = None;
    let finished = wait_for_event(events, timeout_ms, |item| {
        if item.cop_tag.as_deref() != Some(cop_tag) {
            return false;
        }
        match &item.data {
            Some(event_item::Data::ResultData(result)) => {
                response = Some(result.data_bytes.clone());
                false
            }
            Some(event_item::Data::ErrorData(err)) => {
                saw_error = Some(*err);
                false
            }
            Some(event_item::Data::CopStatus(status)) => {
                *status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
                    || *status
                        == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
            }
            _ => false,
        }
    })
    .await;
    if !finished {
        println!("  (timed out waiting for CoptSendrecv completion)");
    } else if let Some(err) = saw_error {
        println!(
            "  (CoptSendrecv for cop_tag={cop_tag:?} reported an async error: \
             PduErrorEvent({err}))"
        );
    }
    response
}

/// Waits for a `CopStatus == Finished` event belonging to the COP identified by
/// `cop_tag` (used after `CoptStartcomm`/`CoptStopcomm`, which don't deliver
/// `ResultData`).
///
/// `cop_tag` must be the exact bytes passed as `StartComPrimitiveRequest.cop_tag`
/// for the call this wait is for (ADR-204). This tag match is required in
/// addition to the `CopStatus == Finished` check so a stale `Finished` event
/// left over from an earlier, already-timed-out primitive (e.g. a slow 5-baud
/// K-line init) can never be accepted as this call's own terminal event.
///
/// An async failure (a K-line initialization error, a J1939 address-claim
/// failure, a lost TP2.0 connection, ...) is surfaced by the service as a
/// tagged `ErrorData` event followed by the same COP's terminal `CopStatus`
/// (`events.rs`'s various initialization-failure paths) -- `ErrorData` is a
/// SIBLING event to `CopStatus`, not a field on it, so it must be tracked
/// separately across the whole wait rather than only inspected at the
/// terminal event. This function does that: an `ErrorData` event matching
/// `cop_tag` is recorded (and printed, so a caller's resulting `Err` isn't a
/// bare "did not complete" with no indication why) but does NOT end the
/// wait by itself -- the terminal `CopStatus` event still arrives afterward
/// and is what actually ends it, same as before.
///
/// Returns `true` only if a matching `Finished` event arrived before
/// `timeout_ms` elapsed AND no matching `ErrorData` was observed along the
/// way; `false` on timeout, on an observed async error, OR on a matching
/// `Cancelled` event (a hard channel failure's own `ErrorData` is emitted
/// WITHOUT a cop_tag, per `events.rs::handle_channel_hard_error`, so it is
/// invisible to this function's tag gate -- `Cancelled` itself is treated
/// as failure regardless, since only `Finished` means the primitive
/// actually completed). Callers
/// gating a subsequent operation on this primitive actually having finished
/// (e.g. a slow multi-second K-line init before a `CoptSendrecv`) must check
/// this return value and abort (return `Err`) on `false` rather than
/// proceeding as if initialization succeeded. For a best-effort wait the
/// caller doesn't depend on (most `CoptStopcomm` waits, which are typically
/// cleanup-only), it's fine to discard the result explicitly with
/// `let _ = ...` -- the one-line `PduErrorEvent` diagnostic below is only
/// printed on the rare path where an error was actually observed (on top of
/// what `wait_for_event`'s own `print_event_item` already prints for every
/// event seen either way), so a discarded result does not spam extra output
/// in the ordinary, no-error case.
#[must_use]
pub async fn wait_for_cop_finished(
    events: &mut Streaming<EventNotification>,
    timeout_ms: u32,
    cop_tag: &[u8],
) -> bool {
    let mut saw_error: Option<i32> = None;
    let mut saw_cancelled = false;
    let terminal_reached = wait_for_event(events, timeout_ms, |item| {
        // A hard channel failure (`events.rs::handle_channel_hard_error`)
        // emits `PduErrEvtLostCommToVci` WITHOUT a cop_tag -- broadcast to
        // every in-flight COP on the failed channel, not attributable to
        // any single one -- followed by a per-COP, correctly-tagged
        // `PduCopstCancelled`. The untagged error is invisible to the
        // `cop_tag` gate below by construction, so `Cancelled` itself must
        // never be treated as a success terminal event: only `Finished`
        // means the primitive actually completed. `saw_cancelled` records
        // this regardless of the tag gate outcome for the diagnostic
        // message below, but only a TAG-MATCHED `Cancelled` ends the wait.
        if item.cop_tag.as_deref() != Some(cop_tag) {
            return false;
        }
        match &item.data {
            Some(event_item::Data::ErrorData(err)) => {
                saw_error = Some(*err);
                false
            }
            Some(event_item::Data::CopStatus(status)) => {
                if *status == vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled as i32
                {
                    saw_cancelled = true;
                    true
                } else {
                    *status == vci_service_interface::PduComPrimitiveStatus::PduCopstFinished as i32
                }
            }
            _ => false,
        }
    })
    .await;
    if let Some(err) = saw_error {
        println!(
            "  (CoptStartcomm/CoptStopcomm for cop_tag={cop_tag:?} reported an async error: \
             PduErrorEvent({err}))"
        );
    } else if saw_cancelled {
        println!(
            "  (CoptStartcomm/CoptStopcomm for cop_tag={cop_tag:?} was Cancelled, not Finished \
             -- typically a hard channel failure whose own error event carries no cop_tag)"
        );
    }
    terminal_reached && saw_error.is_none() && !saw_cancelled
}

/// Tears the link down: `DisconnectComLogicalLink` -> `DestroyComLogicalLink`
/// -> `ModuleDisconnect`, in that order, propagating the first failure.
pub async fn teardown(
    client: &mut VciServiceClient<Channel>,
    cll_handle: ComLogicalLinkHandle,
    module_handle: ModuleHandle,
) -> Result<()> {
    client
        .disconnect_com_logical_link(DisconnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await?;
    client
        .destroy_com_logical_link(DestroyComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await?;
    client
        .module_disconnect(ModuleDisconnectRequest {
            module_handle: Some(module_handle),
        })
        .await?;
    println!("teardown complete");
    Ok(())
}
