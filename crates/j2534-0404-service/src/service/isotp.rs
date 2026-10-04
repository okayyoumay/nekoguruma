//! Software ISO 15765-2 (ISO-TP) transport engine (ADR-046).
//!
//! Used when `can_channel_mode = "software-isotp"`: CAN-family
//! ComLogicalLinks run over a raw CAN J2534 channel and this module performs
//! the USDT segmentation / reassembly that a native ISO15765 J2534 channel
//! would otherwise do in the adapter.
//!
//! Scope (documented limitations):
//! - Normal and extended addressing (a one-byte Address Extension prefix,
//!   ISO 15765-2 / D-PDU API Table B.13 bit 3) are both supported; mixed and
//!   functional addressing are not.
//! - Classic CAN frames (8 data bytes); CAN FD escape encodings are not
//!   supported, so the maximum USDT payload is 4095 bytes (12-bit FF length).
//!
//! This module is pure frame encoding/decoding plus the RX reassembly state
//! machine.  The TX driver (writing frames, waiting for flow control) and the
//! RX integration (fan-out, sending flow control) live in `events.rs`, which
//! owns the J2534 API access.

use std::time::{Duration, Instant};

/// Classic CAN payload size; software ISO-TP frames never exceed this.
pub(super) const CAN_FRAME_DATA_LEN: usize = 8;
/// Maximum segmented-message payload (12-bit first-frame length field);
/// unaffected by addressing mode.
pub(super) const MAX_FF_TOTAL_LEN: usize = 0xFFF;

/// ISO 15765-2 FlowControl `FlowStatus` values.
pub(super) const FS_CONTINUE_TO_SEND: u8 = 0;
pub(super) const FS_WAIT: u8 = 1;
pub(super) const FS_OVERFLOW: u8 = 2;

/// ISO 15765-2 addressing mode used when building or parsing a frame.
///
/// Extended addressing (D-PDU API `CP_Can*Format` Table B.13 bit 3) prepends
/// a one-byte Address Extension (AE) to every ISO-TP frame — SF, FF, CF, and
/// FC alike — reducing the payload capacity of each frame type by 1 byte
/// relative to normal addressing. The AE value itself comes from the paired
/// `CP_Can*ExtAddr` ComParam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Addressing {
    /// The PCI byte is the first CAN data byte.
    Normal,
    /// A one-byte Address Extension precedes the PCI byte.
    Extended(u8),
}

impl Addressing {
    /// Decodes a `CP_Can*Format` UNUM32 bitfield (only Table B.13 bit 3,
    /// Addressing Scheme, is consulted) plus its paired `CP_Can*ExtAddr` low
    /// byte into an `Addressing`. `format_raw = None` (the ComParam was not
    /// set) defaults to `Normal`, matching the addressing-unaware behaviour
    /// this service had before extended-addressing support was added — an
    /// entry that only sets a CAN ID keeps working unchanged.
    pub(super) fn from_format(format_raw: Option<u32>, ext_addr: u8) -> Self {
        match format_raw {
            Some(raw) if raw & 0x08 != 0 => Self::Extended(ext_addr),
            _ => Self::Normal,
        }
    }

    fn ae(self) -> Option<u8> {
        match self {
            Self::Normal => None,
            Self::Extended(b) => Some(b),
        }
    }

    /// Header bytes consumed by the AE, before the PCI byte: 1 if extended, else 0.
    fn ae_len(self) -> usize {
        self.ae().is_some() as usize
    }

    /// Maximum SingleFrame payload for this addressing mode on classic CAN.
    pub(super) fn max_sf_payload(self) -> usize {
        7 - self.ae_len()
    }

    /// Maximum SingleFrame payload for this addressing mode on a CAN FD
    /// frame of `tx_dl` data bytes (ADR-169).
    ///
    /// A Classic CAN Single Frame (`tx_dl <= 8`) uses a one-byte PCI whose
    /// low nibble carries the length, physically capping the payload at
    /// 7/6 bytes (`max_sf_payload()`, unchanged). Once the frame's own data
    /// length exceeds 8 bytes (CAN FD), ISO-TP switches the Single Frame PCI
    /// to a 2-byte escape form — byte `0x00` followed by a full second byte
    /// carrying the actual length — costing one extra header byte versus the
    /// Classic form. The Address Extension byte (Extended addressing)
    /// reduces the result by the same flat 1 byte the Classic form already
    /// subtracts, since the escape trigger is keyed on the frame's own data
    /// length, not on addressing mode.
    ///
    /// This is *not* used by this module's own software-ISO-TP engine, which
    /// is Classic-CAN-only (see this file's module doc comment); it exists
    /// for `rpc_primitive.rs`'s functional-addressing check against a native
    /// `FD_ISO15765_PS` link. See ADR-169 for the derivation and the
    /// spec-availability note on why this rule is cited without a clause
    /// number.
    pub(super) fn fd_max_sf_payload(self, tx_dl: usize) -> usize {
        if tx_dl <= CAN_FRAME_DATA_LEN {
            self.max_sf_payload()
        } else {
            tx_dl - 2 - self.ae_len()
        }
    }

    /// Payload bytes carried by a FirstFrame on classic CAN.
    pub(super) fn ff_payload_len(self) -> usize {
        6 - self.ae_len()
    }

    /// Payload bytes carried by each ConsecutiveFrame on classic CAN.
    pub(super) fn cf_payload_len(self) -> usize {
        7 - self.ae_len()
    }
}

/// One parsed ISO-TP frame (the CAN payload *after* the 4-byte CAN ID that
/// J2534 prepends to message data, and after any Address Extension byte).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Frame<'a> {
    Single {
        payload: &'a [u8],
    },
    First {
        total_len: usize,
        payload: &'a [u8],
    },
    Consecutive {
        sequence_number: u8,
        payload: &'a [u8],
    },
    FlowControl {
        flow_status: u8,
        block_size: u8,
        st_min: u8,
    },
}

/// Parses `data` (CAN payload without the CAN ID) as an ISO-TP frame using
/// `addressing`.  Returns `None` for payloads that are not valid ISO-TP for
/// that addressing mode — including an `Extended` frame whose AE byte does
/// not match the expected value — letting the caller fall back to raw
/// delivery.
pub(super) fn parse_frame(data: &[u8], addressing: Addressing) -> Option<Frame<'_>> {
    let data = match addressing.ae() {
        Some(expected_ae) => {
            if *data.first()? != expected_ae {
                return None;
            }
            &data[1..]
        }
        None => data,
    };
    let &pci = data.first()?;
    match pci >> 4 {
        0x0 => {
            let len = (pci & 0x0F) as usize;
            if len == 0 || len > addressing.max_sf_payload() || data.len() < 1 + len {
                return None;
            }
            Some(Frame::Single {
                payload: &data[1..1 + len],
            })
        }
        0x1 => {
            let hi = (pci & 0x0F) as usize;
            let &lo = data.get(1)?;
            let total_len = (hi << 8) | lo as usize;
            // A first frame must announce more than a single frame can carry.
            if total_len <= addressing.max_sf_payload() {
                return None;
            }
            Some(Frame::First {
                total_len,
                payload: &data[2..],
            })
        }
        0x2 => Some(Frame::Consecutive {
            sequence_number: pci & 0x0F,
            payload: &data[1..],
        }),
        0x3 => {
            let flow_status = pci & 0x0F;
            if flow_status > FS_OVERFLOW {
                return None;
            }
            Some(Frame::FlowControl {
                flow_status,
                block_size: data.get(1).copied().unwrap_or(0),
                st_min: data.get(2).copied().unwrap_or(0),
            })
        }
        _ => None,
    }
}

/// Builds a single-frame CAN payload for `payload` (must be 1 byte up to
/// `addressing.max_sf_payload()`), prefixed with an AE byte when `addressing`
/// is `Extended`.
pub(super) fn single_frame(payload: &[u8], addressing: Addressing) -> Vec<u8> {
    debug_assert!(!payload.is_empty() && payload.len() <= addressing.max_sf_payload());
    let mut data = Vec::with_capacity(CAN_FRAME_DATA_LEN);
    data.extend(addressing.ae());
    data.push(payload.len() as u8);
    data.extend_from_slice(payload);
    data
}

/// Builds the first frame of a segmented message of `total_len` bytes,
/// carrying the first `addressing.ff_payload_len()` bytes of `payload`,
/// prefixed with an AE byte when `addressing` is `Extended`.
pub(super) fn first_frame(total_len: usize, payload: &[u8], addressing: Addressing) -> Vec<u8> {
    debug_assert!(total_len > addressing.max_sf_payload() && total_len <= MAX_FF_TOTAL_LEN);
    let mut data = Vec::with_capacity(CAN_FRAME_DATA_LEN);
    data.extend(addressing.ae());
    data.push(0x10 | ((total_len >> 8) as u8 & 0x0F));
    data.push((total_len & 0xFF) as u8);
    let ff_len = addressing.ff_payload_len();
    data.extend_from_slice(&payload[..ff_len.min(payload.len())]);
    data
}

/// Builds a consecutive frame with sequence number `sn` (low nibble used),
/// prefixed with an AE byte when `addressing` is `Extended`.
pub(super) fn consecutive_frame(sn: u8, chunk: &[u8], addressing: Addressing) -> Vec<u8> {
    debug_assert!(!chunk.is_empty() && chunk.len() <= addressing.cf_payload_len());
    let mut data = Vec::with_capacity(CAN_FRAME_DATA_LEN);
    data.extend(addressing.ae());
    data.push(0x20 | (sn & 0x0F));
    data.extend_from_slice(chunk);
    data
}

/// Builds a FlowControl frame, prefixed with an AE byte when `addressing` is
/// `Extended`.
pub(super) fn flow_control_frame(
    flow_status: u8,
    block_size: u8,
    st_min: u8,
    addressing: Addressing,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(4);
    data.extend(addressing.ae());
    data.push(0x30 | (flow_status & 0x0F));
    data.push(block_size);
    data.push(st_min);
    data
}

/// Pads `frame` to the full classic-CAN length with `filler`.
pub(super) fn pad_frame(frame: &mut Vec<u8>, filler: u8) {
    frame.resize(frame.len().max(CAN_FRAME_DATA_LEN), filler);
}

/// Decodes an ISO 15765-2 STmin byte into the minimum consecutive-frame gap.
///
/// `0x00`–`0x7F`: 0–127 ms.  `0xF1`–`0xF9`: 100–900 µs.  Reserved values are
/// treated as the specified maximum (127 ms) per ISO 15765-2 §9.6.5.4.
pub(super) fn st_min_delay(st_min: u8) -> Duration {
    match st_min {
        0x00..=0x7F => Duration::from_millis(st_min as u64),
        0xF1..=0xF9 => Duration::from_micros((st_min - 0xF0) as u64 * 100),
        _ => Duration::from_millis(127),
    }
}

/// Splits the post-first-frame remainder of a TX payload into consecutive
/// frame chunks sized for `addressing`, pairing each with its sequence
/// number (starting at 1, wrapping 15 → 0).
pub(super) fn consecutive_chunks(
    payload: &[u8],
    addressing: Addressing,
) -> impl Iterator<Item = (u8, &[u8])> {
    let ff_len = addressing.ff_payload_len();
    let cf_len = addressing.cf_payload_len();
    payload[ff_len.min(payload.len())..]
        .chunks(cf_len)
        .enumerate()
        .map(|(i, chunk)| (((i + 1) % 16) as u8, chunk))
}

/// Outcome of feeding one consecutive frame into a [`Reassembly`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReassemblyStep {
    /// Message complete; contains the full reassembled payload.
    Complete(Vec<u8>),
    /// More consecutive frames are expected.
    Continue,
    /// The frame's sequence number did not match; reassembly was aborted.
    WrongSequence,
    /// N_Cr expired before this frame arrived; reassembly was aborted.
    TimedOut,
}

/// RX reassembly state for one in-progress segmented message (per CAN ID).
#[derive(Debug)]
pub(super) struct Reassembly {
    total_len: usize,
    buf: Vec<u8>,
    next_sn: u8,
    /// Instant after which waiting for the next consecutive frame is
    /// abandoned (ISO 15765-2 N_Cr).
    deadline: Instant,
    n_cr: Duration,
}

impl Reassembly {
    /// Starts reassembly from a first frame.
    pub(super) fn start(
        total_len: usize,
        first_payload: &[u8],
        now: Instant,
        n_cr: Duration,
    ) -> Self {
        let mut buf = Vec::with_capacity(total_len);
        buf.extend_from_slice(&first_payload[..first_payload.len().min(total_len)]);
        Self {
            total_len,
            buf,
            next_sn: 1,
            deadline: now + n_cr,
            n_cr,
        }
    }

    /// Feeds a consecutive frame.  On success the N_Cr deadline is re-armed.
    pub(super) fn on_consecutive(
        &mut self,
        sn: u8,
        payload: &[u8],
        now: Instant,
    ) -> ReassemblyStep {
        if now > self.deadline {
            return ReassemblyStep::TimedOut;
        }
        if sn != self.next_sn {
            return ReassemblyStep::WrongSequence;
        }
        self.next_sn = (self.next_sn + 1) % 16;
        let remaining = self.total_len - self.buf.len();
        self.buf
            .extend_from_slice(&payload[..payload.len().min(remaining)]);
        if self.buf.len() >= self.total_len {
            ReassemblyStep::Complete(std::mem::take(&mut self.buf))
        } else {
            self.deadline = now + self.n_cr;
            ReassemblyStep::Continue
        }
    }

    /// Returns `true` when N_Cr has expired with the message still
    /// incomplete (the entry should be dropped).
    pub(super) fn is_expired(&self, now: Instant) -> bool {
        now > self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_frame() {
        assert_eq!(
            parse_frame(
                &[0x03, 0x22, 0xF1, 0x90, 0xAA, 0xAA, 0xAA, 0xAA],
                Addressing::Normal
            ),
            Some(Frame::Single {
                payload: &[0x22, 0xF1, 0x90]
            })
        );
    }

    #[test]
    fn rejects_invalid_single_frame_lengths() {
        assert_eq!(parse_frame(&[0x00, 0x01], Addressing::Normal), None); // SF length 0
        assert_eq!(parse_frame(&[0x05, 0x01, 0x02], Addressing::Normal), None); // shorter than announced
        assert_eq!(parse_frame(&[], Addressing::Normal), None);
    }

    #[test]
    fn parses_first_frame() {
        assert_eq!(
            parse_frame(
                &[0x10, 0x0A, 0x62, 0xF1, 0x90, 0x01, 0x02, 0x03],
                Addressing::Normal
            ),
            Some(Frame::First {
                total_len: 10,
                payload: &[0x62, 0xF1, 0x90, 0x01, 0x02, 0x03]
            })
        );
        // 12-bit length spans both nibble and second byte.
        assert_eq!(
            parse_frame(&[0x1F, 0xFF, 0, 0, 0, 0, 0, 0], Addressing::Normal),
            Some(Frame::First {
                total_len: 0xFFF,
                payload: &[0, 0, 0, 0, 0, 0]
            })
        );
    }

    #[test]
    fn rejects_first_frame_that_fits_in_single_frame() {
        assert_eq!(
            parse_frame(&[0x10, 0x07, 1, 2, 3, 4, 5, 6], Addressing::Normal),
            None
        );
    }

    #[test]
    fn parses_consecutive_and_flow_control() {
        assert_eq!(
            parse_frame(&[0x21, 0x04, 0x05], Addressing::Normal),
            Some(Frame::Consecutive {
                sequence_number: 1,
                payload: &[0x04, 0x05]
            })
        );
        assert_eq!(
            parse_frame(&[0x30, 0x08, 0x14], Addressing::Normal),
            Some(Frame::FlowControl {
                flow_status: FS_CONTINUE_TO_SEND,
                block_size: 8,
                st_min: 0x14
            })
        );
        assert_eq!(parse_frame(&[0x33], Addressing::Normal), None); // reserved flow status
        assert_eq!(parse_frame(&[0x40, 0x00], Addressing::Normal), None); // unknown PCI type
    }

    #[test]
    fn builds_frames_that_roundtrip() {
        let sf = single_frame(&[0x3E, 0x00], Addressing::Normal);
        assert_eq!(
            parse_frame(&sf, Addressing::Normal),
            Some(Frame::Single {
                payload: &[0x3E, 0x00]
            })
        );

        let payload: Vec<u8> = (0..20).collect();
        let ff = first_frame(payload.len(), &payload, Addressing::Normal);
        assert_eq!(
            parse_frame(&ff, Addressing::Normal),
            Some(Frame::First {
                total_len: 20,
                payload: &payload[..6]
            })
        );

        let cf = consecutive_frame(3, &payload[6..13], Addressing::Normal);
        assert_eq!(
            parse_frame(&cf, Addressing::Normal),
            Some(Frame::Consecutive {
                sequence_number: 3,
                payload: &payload[6..13]
            })
        );

        let fc = flow_control_frame(FS_CONTINUE_TO_SEND, 4, 0xF3, Addressing::Normal);
        assert_eq!(
            parse_frame(&fc, Addressing::Normal),
            Some(Frame::FlowControl {
                flow_status: 0,
                block_size: 4,
                st_min: 0xF3
            })
        );
    }

    #[test]
    fn pads_to_classic_can_length() {
        let mut frame = single_frame(&[0x3E, 0x00], Addressing::Normal);
        pad_frame(&mut frame, 0xCC);
        assert_eq!(frame, vec![0x02, 0x3E, 0x00, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC]);
    }

    #[test]
    fn decodes_addressing_from_can_format_bitfield() {
        // Bit 3 clear (or format absent): normal addressing regardless of ext_addr.
        assert_eq!(Addressing::from_format(None, 0xAB), Addressing::Normal);
        assert_eq!(
            Addressing::from_format(Some(0x02), 0xAB),
            Addressing::Normal
        );
        // Bit 3 set: extended addressing, AE = ext_addr low byte.
        assert_eq!(
            Addressing::from_format(Some(0x08), 0xAB),
            Addressing::Extended(0xAB)
        );
        assert_eq!(
            Addressing::from_format(Some(0x0B), 0x00),
            Addressing::Extended(0x00)
        );
    }

    #[test]
    fn extended_addressing_capacities_are_one_byte_smaller() {
        let ext = Addressing::Extended(0x01);
        assert_eq!(ext.max_sf_payload(), 6);
        assert_eq!(ext.ff_payload_len(), 5);
        assert_eq!(ext.cf_payload_len(), 6);
        assert_eq!(Addressing::Normal.max_sf_payload(), 7);
        assert_eq!(Addressing::Normal.ff_payload_len(), 6);
        assert_eq!(Addressing::Normal.cf_payload_len(), 7);
    }

    #[test]
    fn fd_max_sf_payload_matches_adr_169_table() {
        // (tx_dl, expected Normal max SF, expected Extended max SF)
        const CASES: &[(usize, usize, usize)] = &[
            (0, 7, 6),
            (8, 7, 6),
            (12, 10, 9),
            (16, 14, 13),
            (20, 18, 17),
            (24, 22, 21),
            (32, 30, 29),
            (48, 46, 45),
            (64, 62, 61),
        ];
        let ext = Addressing::Extended(0x01);
        for &(tx_dl, normal_expected, extended_expected) in CASES {
            assert_eq!(
                Addressing::Normal.fd_max_sf_payload(tx_dl),
                normal_expected,
                "Normal addressing, tx_dl={tx_dl}"
            );
            assert_eq!(
                ext.fd_max_sf_payload(tx_dl),
                extended_expected,
                "Extended addressing, tx_dl={tx_dl}"
            );
        }
    }

    #[test]
    fn builds_and_parses_extended_addressing_frames() {
        let addressing = Addressing::Extended(0xF1);

        let sf = single_frame(&[0x22, 0xF1, 0x90], addressing);
        assert_eq!(sf, vec![0xF1, 0x03, 0x22, 0xF1, 0x90]);
        assert_eq!(
            parse_frame(&sf, addressing),
            Some(Frame::Single {
                payload: &[0x22, 0xF1, 0x90]
            })
        );

        let payload: Vec<u8> = (0..20).collect();
        let ff = first_frame(payload.len(), &payload, addressing);
        // [AE][PCI hi/lo][5 payload bytes] = 8 bytes on classic CAN.
        assert_eq!(ff.len(), 8);
        assert_eq!(
            parse_frame(&ff, addressing),
            Some(Frame::First {
                total_len: 20,
                payload: &payload[..5]
            })
        );

        let cf = consecutive_frame(1, &payload[5..11], addressing);
        assert_eq!(
            parse_frame(&cf, addressing),
            Some(Frame::Consecutive {
                sequence_number: 1,
                payload: &payload[5..11]
            })
        );

        let fc = flow_control_frame(FS_CONTINUE_TO_SEND, 8, 0x0A, addressing);
        assert_eq!(fc, vec![0xF1, 0x30, 0x08, 0x0A]);
        assert_eq!(
            parse_frame(&fc, addressing),
            Some(Frame::FlowControl {
                flow_status: 0,
                block_size: 8,
                st_min: 0x0A
            })
        );
    }

    #[test]
    fn extended_addressing_rejects_mismatched_ae_byte() {
        let sf = single_frame(&[0x22, 0xF1, 0x90], Addressing::Extended(0xF1));
        // A frame addressed to a different target (different AE) must not parse.
        assert_eq!(parse_frame(&sf, Addressing::Extended(0xF2)), None);
        // Nor should a normal-addressing parse of an extended-addressing frame
        // silently succeed by misreading the AE byte as the PCI byte (0xF1
        // would decode as an invalid PCI type 0xF).
        assert_eq!(parse_frame(&sf, Addressing::Normal), None);
    }

    #[test]
    fn st_min_decoding() {
        assert_eq!(st_min_delay(0x00), Duration::ZERO);
        assert_eq!(st_min_delay(0x7F), Duration::from_millis(127));
        assert_eq!(st_min_delay(0xF1), Duration::from_micros(100));
        assert_eq!(st_min_delay(0xF9), Duration::from_micros(900));
        // Reserved values fall back to the maximum.
        assert_eq!(st_min_delay(0x80), Duration::from_millis(127));
        assert_eq!(st_min_delay(0xFA), Duration::from_millis(127));
    }

    #[test]
    fn consecutive_chunks_sequence_numbers_wrap() {
        // 6 (FF) + 16 * 7 = 118 bytes: SNs 1..=15, then 0, then 1.
        let payload: Vec<u8> = (0..125u8).collect();
        let chunks: Vec<(u8, usize)> = consecutive_chunks(&payload, Addressing::Normal)
            .map(|(sn, c)| (sn, c.len()))
            .collect();
        assert_eq!(chunks.len(), 17);
        assert_eq!(chunks[0].0, 1);
        assert_eq!(chunks[14].0, 15);
        assert_eq!(chunks[15].0, 0);
        assert_eq!(chunks[16], (1, 119 - 16 * 7));
    }

    #[test]
    fn consecutive_chunks_use_smaller_sizes_under_extended_addressing() {
        // 5 (FF) + 6 + 6 + 3 = 20 bytes: three chunks, last one partial.
        let payload: Vec<u8> = (0..20u8).collect();
        let chunks: Vec<(u8, usize)> = consecutive_chunks(&payload, Addressing::Extended(0x01))
            .map(|(sn, c)| (sn, c.len()))
            .collect();
        assert_eq!(chunks, vec![(1, 6), (2, 6), (3, 3)]);
    }

    #[test]
    fn reassembles_segmented_message() {
        let now = Instant::now();
        let n_cr = Duration::from_millis(1000);
        let payload: Vec<u8> = (0..20).collect();

        let mut r = Reassembly::start(20, &payload[..6], now, n_cr);
        assert_eq!(
            r.on_consecutive(1, &payload[6..13], now),
            ReassemblyStep::Continue
        );
        assert_eq!(
            r.on_consecutive(2, &payload[13..20], now),
            ReassemblyStep::Complete(payload.clone())
        );
    }

    #[test]
    fn reassembly_ignores_padding_beyond_total_len() {
        let now = Instant::now();
        let n_cr = Duration::from_millis(1000);
        // 9-byte message: FF carries 6, CF carries 3 + 4 padding bytes.
        let mut r = Reassembly::start(9, &[1, 2, 3, 4, 5, 6], now, n_cr);
        assert_eq!(
            r.on_consecutive(1, &[7, 8, 9, 0xAA, 0xAA, 0xAA, 0xAA], now),
            ReassemblyStep::Complete(vec![1, 2, 3, 4, 5, 6, 7, 8, 9])
        );
    }

    #[test]
    fn reassembly_detects_wrong_sequence_and_timeout() {
        let now = Instant::now();
        let n_cr = Duration::from_millis(10);
        let mut r = Reassembly::start(20, &[0; 6], now, n_cr);
        assert_eq!(
            r.on_consecutive(2, &[0; 7], now),
            ReassemblyStep::WrongSequence
        );

        let mut r = Reassembly::start(20, &[0; 6], now, n_cr);
        let late = now + Duration::from_millis(11);
        assert!(r.is_expired(late));
        assert_eq!(r.on_consecutive(1, &[0; 7], late), ReassemblyStep::TimedOut);
    }
}
