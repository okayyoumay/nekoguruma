//! Demonstrates the native `j2534-0404` channel read/write flow against a
//! real J2534 v04.04 device: load the vendor DLL, open the device, connect a
//! channel, write a request message, poll for a reply, then disconnect and
//! close the device.
//!
//! Requires a physical J2534-compatible interface (e.g. an OpenPort 2.0)
//! with its vendor DLL installed and, ideally, a vehicle or bench harness
//! connected so a reply can be observed on the channel.
//!
//! Usage: channel_read_write_0404 <path-to-j2534-dll> [protocol-id] [baud-rate]
//! Example: channel_read_write_0404 C:\Vendor\J2534.dll
//!
//! `protocol-id` and `baud-rate` default to ISO15765 (CAN) at 500000 baud,
//! the same defaults `j2534-0404/tests/live_channel.rs` uses. For the CAN
//! and ISO15765 protocol families, `Data` must start with the 4-byte
//! big-endian CAN ID (SAE J2534-1 §8); `REQUEST_DATA` below is laid out for
//! the default `protocol-id`, and a non-default `protocol-id` argument needs
//! a protocol-appropriate `Data` layout of its own, which this example does
//! not attempt to build generically.
//!
//! A real bus is noisy: unrelated ISO15765 traffic can arrive interleaved
//! with the actual reply, and if the adapter's `CONFIG_LOOPBACK`/TxDone
//! indication is enabled, the just-transmitted `REQUEST_DATA` frame itself
//! can come back through `PassThruReadMsgs` before a real response does. A
//! single one-message read would happily accept whichever of those arrives
//! first and report it as "the" response. This example instead reads in a
//! loop until `READ_TIMEOUT_MS` elapses, discarding (via
//! `is_expected_response` below) any frame that is a loopback echo/TxDone
//! indication of our own transmit, or -- for the default ISO15765 case,
//! where `REQUEST_DATA`'s own CAN-ID layout makes this possible -- any frame
//! whose CAN ID isn't the expected `RESPONSE_CAN_ID`.

use j2534_0404::{J2534Api0404, PassThruMessage, StatusCode};

const WRITE_TIMEOUT_MS: u32 = 200;
const READ_TIMEOUT_MS: u32 = 50;
const REQUEST_DATA: [u8; 6] = [0x00, 0x00, 0x07, 0xE0, 0x3E, 0x00];
// Standard 11-bit UDS physical response ID for REQUEST_DATA's own CAN ID
// (0x7E0) -- only meaningful for the default ISO15765 protocol-id, same
// caveat as REQUEST_DATA's own module-doc-comment note above.
const RESPONSE_CAN_ID: u32 = 0x7E8;

/// `RX_TX_MSG_TYPE` (SAE J2534-1 RxStatus bit `0x00000001`, §8.7's Message
/// Flag and Status Definitions table) -- set when a received frame is either
/// a loopback echo of a message this device itself transmitted
/// (`CONFIG_LOOPBACK`) or an ISO15765 TxDone indication (a separate table in
/// the same §8.7 defines a TxDone indication as `TX_MSG_TYPE = 1, TX_DONE =
/// 1`); either way, not genuine external bus traffic. Not present in
/// `j2534-0404-sys`'s generated
/// bindings -- bindgen's `allowlist_var` regex matches `RX_FLAG_.*` but not
/// this bare `RX_TX_MSG_TYPE` header name, and widening that regex would
/// mean regenerating committed bindings for all 5 pre-committed targets via
/// `--features bindgen`, out of scope for this fix -- so this mirrors
/// `j2534-0404-service/src/service/events.rs`'s own identical local-const
/// workaround for the same gap, rather than requesting a regen for one bit
/// value the J2534 v04.04 header already defines.
const RX_TX_MSG_TYPE: u32 = 0x0000_0001;

/// True when `message` looks like a genuine response to `REQUEST_DATA`
/// rather than a loopback/TxDone echo of our own transmit, or -- for the
/// default ISO15765 `protocol_id` -- unrelated CAN traffic on the bus. For a
/// non-default `protocol_id`, only the loopback/TxDone check applies (SAE
/// J2534-1's `TX_MSG_TYPE` bit is defined for every protocol family); this
/// example doesn't know that protocol's own `Data` layout well enough to
/// filter by ID, matching the module doc comment's non-default-protocol-id
/// caveat.
fn is_expected_response(message: &PassThruMessage, protocol_id: u32) -> bool {
    if message.rx_status() & RX_TX_MSG_TYPE != 0 {
        return false;
    }
    if protocol_id != j2534_0404::ISO15765 {
        return true;
    }
    match message.data() {
        Ok(data) => data
            .get(..4)
            .is_some_and(|prefix| prefix == RESPONSE_CAN_ID.to_be_bytes().as_slice()),
        Err(_) => false,
    }
}

fn usage(bin: &str) {
    eprintln!("Usage: {bin} <path-to-j2534-dll> [protocol-id] [baud-rate]");
    eprintln!("Example: {bin} C:\\Vendor\\J2534.dll");
}

fn parse_u32_arg(bin: &str, name: &str, raw: &str) -> u32 {
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
            usage(bin);
            std::process::exit(2);
        }
    }
}

fn main() {
    let mut args = std::env::args();
    let bin = args
        .next()
        .unwrap_or_else(|| "channel_read_write_0404".to_string());
    let dll_path = match args.next() {
        Some(path) => path,
        None => {
            usage(&bin);
            std::process::exit(2);
        }
    };
    let protocol_id = args
        .next()
        .map(|raw| parse_u32_arg(&bin, "protocol-id", &raw))
        .unwrap_or(j2534_0404::ISO15765);
    let baud_rate = args
        .next()
        .map(|raw| parse_u32_arg(&bin, "baud-rate", &raw))
        .unwrap_or(500_000);

    let api = match J2534Api0404::from_path(&dll_path) {
        Ok(api) => api,
        Err(err) => {
            eprintln!("failed to load DLL {dll_path}: {err}");
            std::process::exit(1);
        }
    };

    let device_id = match api.open(None) {
        Ok(device_id) => device_id,
        Err(err) => {
            eprintln!("PassThruOpen failed: {err}");
            std::process::exit(1);
        }
    };

    let channel_id = match api.connect(device_id, protocol_id, 0, baud_rate) {
        Ok(channel_id) => channel_id,
        Err(err) => {
            eprintln!("PassThruConnect failed: {err}");
            let _ = api.close(device_id);
            std::process::exit(1);
        }
    };

    let mut tx_message = match PassThruMessage::new(
        protocol_id,
        0,
        j2534_0404::TX_NORMAL_TRANSMIT,
        0,
        0,
        &REQUEST_DATA,
    ) {
        Ok(message) => message,
        Err(err) => {
            eprintln!("failed to build request message: {err}");
            let _ = api.disconnect(channel_id);
            let _ = api.close(device_id);
            std::process::exit(1);
        }
    };

    let written = match api.write_messages(
        channel_id,
        std::slice::from_mut(&mut tx_message),
        WRITE_TIMEOUT_MS,
    ) {
        Ok(written) => written,
        Err(err) => {
            eprintln!("PassThruWriteMsgs failed: {err}");
            let _ = api.disconnect(channel_id);
            let _ = api.close(device_id);
            std::process::exit(1);
        }
    };
    println!("wrote {written} message(s)");

    let read_deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(READ_TIMEOUT_MS as u64);
    let mut response: Option<PassThruMessage> = None;
    loop {
        let remaining_ms = read_deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .try_into()
            .unwrap_or(u32::MAX);
        if remaining_ms == 0 {
            break;
        }
        // max_messages=1, not a larger batch: per SAE J2534-1 Figure 12,
        // PassThruReadMsgs with a nonzero (blocking) Timeout and
        // 0 < NumMsgs < the requested count returns ERR_TIMEOUT, not
        // STATUS_NOERROR -- read_messages (lib.rs) maps that to Err and
        // discards the partially-filled batch entirely, INCLUDING a real
        // response that arrived alongside a loopback/TxDone frame. Requesting
        // exactly 1 avoids that path: with max_messages=1, a nonzero timeout
        // can only ever resolve to STATUS_NOERROR (1 message) or
        // ERR_BUFFER_EMPTY (0), never a discarded partial fill (an
        // edge-case-hunter review of an earlier max_messages=8 version of
        // this loop caught this exact regression before it shipped).
        match api.read_messages(channel_id, 1, remaining_ms) {
            Ok(messages) if messages.is_empty() => break,
            Ok(messages) => {
                if let Some(found) = messages
                    .into_iter()
                    .find(|m| is_expected_response(m, protocol_id))
                {
                    response = Some(found);
                    break;
                }
                // This message was a loopback/TxDone echo or unrelated
                // traffic -- keep reading until READ_TIMEOUT_MS elapses.
            }
            Err(j2534_0404::Error::ApiStatus { code, .. })
                if code == StatusCode(j2534_0404::ERR_BUFFER_EMPTY) =>
            {
                break;
            }
            Err(err) => {
                eprintln!("PassThruReadMsgs failed: {err}");
                let _ = api.disconnect(channel_id);
                let _ = api.close(device_id);
                std::process::exit(1);
            }
        }
    }
    match response {
        Some(message) => match message.data() {
            Ok(data) => println!("received {} byte(s): {data:02X?}", data.len()),
            Err(err) => eprintln!("received message had invalid data: {err}"),
        },
        None => println!("no matching message received within {READ_TIMEOUT_MS}ms"),
    }

    if let Err(err) = api.disconnect(channel_id) {
        eprintln!("PassThruDisconnect failed: {err}");
        std::process::exit(1);
    }

    if let Err(err) = api.close(device_id) {
        eprintln!("PassThruClose failed: {err}");
        std::process::exit(1);
    }
}
