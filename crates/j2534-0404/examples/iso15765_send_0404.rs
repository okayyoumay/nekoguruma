//! Demonstrates the ISO-TP frame builder helpers in `j2534-0404::iso15765`
//! against a real J2534 v04.04 device: connect an ISO15765 (CAN) channel,
//! build a native-ISO15765 write with `single_frame`/`single_frame_extended`
//! (`Data` is the 4-byte big-endian CAN ID followed by the raw payload; the
//! adapter inserts ISO-TP PCI/framing transparently), enable device-side
//! zero-padding with `with_padding`, and write it to the bus.
//!
//! Requires a physical J2534-compatible interface (e.g. an OpenPort 2.0)
//! with its vendor DLL installed and connected to a CAN bus (a vehicle or
//! bench harness) so the frame can actually be transmitted.
//!
//! Usage: iso15765_send_0404 <path-to-j2534-dll> [normal11|extended29] [can-id] [baud-rate]
//! Example: iso15765_send_0404 C:\Vendor\J2534.dll normal11 0x7E0
//!
//! `can-id` and `baud-rate` default to 0x7E0 and 500000, the same defaults
//! `j2534-0404/tests/live_iso15765.rs` uses. `normal11` builds an 11-bit
//! normal-addressing frame; `extended29` builds a 29-bit frame with the
//! `TX_EXTENDED_ID` flag set.

use j2534_0404::{
    J2534Api0404,
    iso15765::{single_frame, single_frame_extended, with_padding},
};

const REQUEST_PAYLOAD: [u8; 2] = [0x10, 0x03];
const WRITE_TIMEOUT_MS: u32 = 200;

fn usage(bin: &str) {
    eprintln!("Usage: {bin} <path-to-j2534-dll> [normal11|extended29] [can-id] [baud-rate]");
    eprintln!("Example: {bin} C:\\Vendor\\J2534.dll normal11 0x7E0");
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
        .unwrap_or_else(|| "iso15765_send_0404".to_string());
    let dll_path = match args.next() {
        Some(path) => path,
        None => {
            usage(&bin);
            std::process::exit(2);
        }
    };
    let mode = args.next().unwrap_or_else(|| "normal11".to_string());
    if mode != "normal11" && mode != "extended29" {
        eprintln!("invalid mode '{mode}'; expected normal11 or extended29");
        usage(&bin);
        std::process::exit(2);
    }
    let can_id = args
        .next()
        .map(|raw| parse_u32_arg(&bin, "can-id", &raw))
        .unwrap_or(0x7E0);
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

    let channel_id = match api.connect(device_id, j2534_0404::ISO15765, 0, baud_rate) {
        Ok(channel_id) => channel_id,
        Err(err) => {
            eprintln!("PassThruConnect failed: {err}");
            let _ = api.close(device_id);
            std::process::exit(1);
        }
    };

    let built = if mode == "extended29" {
        single_frame_extended(can_id, &REQUEST_PAYLOAD)
    } else {
        single_frame(can_id, &REQUEST_PAYLOAD)
    };
    let message = match built {
        Ok(message) => message,
        Err(err) => {
            eprintln!("failed to build {mode} single frame: {err}");
            let _ = api.disconnect(channel_id);
            let _ = api.close(device_id);
            std::process::exit(1);
        }
    };
    let mut tx_message = with_padding(message);

    println!(
        "sending {mode} frame: can_id=0x{can_id:X} tx_flags=0x{:X} data={:02X?}",
        tx_message.tx_flags(),
        tx_message.data().unwrap_or(&[])
    );

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

    if let Err(err) = api.disconnect(channel_id) {
        eprintln!("PassThruDisconnect failed: {err}");
        std::process::exit(1);
    }

    if let Err(err) = api.close(device_id) {
        eprintln!("PassThruClose failed: {err}");
        std::process::exit(1);
    }
}
