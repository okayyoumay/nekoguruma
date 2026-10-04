use std::{
    borrow::{Borrow, ToOwned},
    mem::MaybeUninit,
    ops::Deref,
};

use j2534_0404_sys::bindings::{NDIS_ADAPTER_INFORMATION, PASSTHRU_MSG, REPEAT_MSG_SETUP};

use crate::{Error, MAX_MESSAGE_DATA};

/// Borrowed, read-only wrapper around a native `PASSTHRU_MSG`.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedPassThruMessage(pub PASSTHRU_MSG);

impl BorrowedPassThruMessage {
    /// Returns the protocol identifier.
    pub fn protocol_id(&self) -> u32 {
        self.0.ProtocolID
    }

    /// Returns receiver status flags.
    pub fn rx_status(&self) -> u32 {
        self.0.RxStatus
    }

    /// Returns transmit flags.
    pub fn tx_flags(&self) -> u32 {
        self.0.TxFlags
    }

    /// Returns the device timestamp.
    pub fn timestamp(&self) -> u32 {
        self.0.Timestamp
    }

    /// Returns the native extra-data index.
    pub fn extra_data_index(&self) -> u32 {
        self.0.ExtraDataIndex
    }

    /// Returns payload length after validating native size.
    pub fn data_size(&self) -> Result<usize, Error> {
        validate_data_size(self.0.DataSize as usize)
    }

    /// Returns payload bytes after validating native size.
    pub fn data(&self) -> Result<&[u8], Error> {
        let size = self.data_size()?;
        Ok(&self.0.Data[..size])
    }
}

impl ToOwned for BorrowedPassThruMessage {
    type Owned = PassThruMessage;

    fn to_owned(&self) -> Self::Owned {
        PassThruMessage(self.0)
    }
}

/// Owned wrapper around `PASSTHRU_MSG` with helper methods.
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct PassThruMessage(pub PASSTHRU_MSG);

impl PassThruMessage {
    /// Creates a new message after validating payload length.
    pub fn new(
        protocol_id: u32,
        rx_status: u32,
        tx_flags: u32,
        timestamp: u32,
        extra_data_index: u32,
        data: &[u8],
    ) -> Result<Self, Error> {
        let size = validate_data_size(data.len())?;
        let mut raw = PASSTHRU_MSG {
            ProtocolID: protocol_id,
            RxStatus: rx_status,
            TxFlags: tx_flags,
            Timestamp: timestamp,
            DataSize: size as u32,
            ExtraDataIndex: extra_data_index,
            Data: [0_u8; MAX_MESSAGE_DATA],
        };
        raw.Data[..size].copy_from_slice(data);
        Ok(Self(raw))
    }

    pub(crate) fn zeroed() -> Self {
        Self(unsafe { MaybeUninit::<PASSTHRU_MSG>::zeroed().assume_init() })
    }

    pub(crate) fn as_raw_mut_ptr(&mut self) -> *mut PASSTHRU_MSG {
        &mut self.0
    }

    /// Returns a borrowed view over this message.
    pub fn borrowed(&self) -> &BorrowedPassThruMessage {
        unsafe { &*(self as *const PassThruMessage as *const BorrowedPassThruMessage) }
    }

    /// Replaces transmit flags.
    pub fn set_tx_flags(&mut self, flags: u32) {
        self.0.TxFlags = flags;
    }

    /// Pads payload to `len` using `fill`.
    ///
    /// Returns an error if `len` is smaller than the current payload.
    ///
    /// Not what a native ISO15765 write needs for wire-level frame padding: the
    /// device pads the transmitted CAN frame(s) itself once `TX_ISO15765_FRAME_PAD` is
    /// set (see `iso15765::with_padding`), so padding the logical `Data` buffer here
    /// would extend the payload instead. Only call this for a protocol/mode where the
    /// caller genuinely owns the on-wire frame length.
    pub fn pad_data_to_length(&mut self, len: usize, fill: u8) -> Result<(), Error> {
        let current = self.data_size()?;
        let target = validate_data_size(len)?;
        if target < current {
            return Err(Error::InvalidArgument {
                name: "len",
                reason: "pad target length cannot be smaller than current payload length",
            });
        }
        self.0.Data[current..target].fill(fill);
        self.0.DataSize = target as u32;
        Ok(())
    }
}

impl Deref for PassThruMessage {
    type Target = BorrowedPassThruMessage;

    fn deref(&self) -> &Self::Target {
        self.borrowed()
    }
}

impl Borrow<BorrowedPassThruMessage> for PassThruMessage {
    fn borrow(&self) -> &BorrowedPassThruMessage {
        self.borrowed()
    }
}

impl PartialEq for PassThruMessage {
    fn eq(&self, other: &Self) -> bool {
        self.protocol_id() == other.protocol_id()
            && self.rx_status() == other.rx_status()
            && self.tx_flags() == other.tx_flags()
            && self.timestamp() == other.timestamp()
            && self.extra_data_index() == other.extra_data_index()
            && self.data().ok() == other.data().ok()
    }
}

impl Eq for PassThruMessage {}

/// Owned wrapper around `REPEAT_MSG_SETUP` (SAE J2534-2 clause 14 Repeat
/// Messaging, ADR-165/Phase 12) -- mirrors [`PassThruMessage`]'s
/// construction shape, one level up. `RepeatMsgData[0]` is the message the
/// device autonomously retransmits at `time_interval`; `[1]`/`[2]` are the
/// mask/pattern `PASSTHRU_MSG` pair the device evaluates against incoming
/// frames for `Condition = 1`'s stop condition (the same mask/pattern
/// convention `PassThruStartMsgFilter` uses, one struct level up since this
/// native call takes a single `REPEAT_MSG_SETUP*` rather than three separate
/// pointers).
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct RepeatMsgSetup(pub(crate) REPEAT_MSG_SETUP);

impl RepeatMsgSetup {
    /// Builds a new `REPEAT_MSG_SETUP` from `time_interval` (ms),
    /// `condition` (the raw native `Condition` value), the repeat message
    /// itself, and its stop-condition mask/pattern pair. Each `PassThruMessage`
    /// is already validated by its own `PassThruMessage::new` construction.
    pub fn new(
        time_interval: u32,
        condition: u32,
        message: PassThruMessage,
        mask: PassThruMessage,
        pattern: PassThruMessage,
    ) -> Self {
        Self(REPEAT_MSG_SETUP {
            TimeInterval: time_interval,
            Condition: condition,
            RepeatMsgData: [message.0, mask.0, pattern.0],
        })
    }

    pub(crate) fn as_raw_mut_ptr(&mut self) -> *mut REPEAT_MSG_SETUP {
        &mut self.0
    }
}

/// Owned wrapper around `NDIS_ADAPTER_INFORMATION` (SAE J2534-2 clause 24
/// Ethernet_NDIS, ADR-194/Phase 16) -- the channel-scoped, no-input struct
/// output of `PassThruIoctl(GET_NDIS_ADAPTER_INFO)`. Mirrors
/// [`PassThruMessage`]'s owned-wrapper-over-a-raw-`#[repr(C)]`-struct shape
/// one level up. `AdapterUniqueID`/`AdapterName` are native
/// `std::os::raw::c_char` arrays; exposed here as raw `u8` byte spans (not a
/// `CStr`/C-string interpretation) since `j2534-0404-service`'s `rpc_misc.rs`
/// hand-packs them into `DataItem.bytearray_data` byte-for-byte, null-padded
/// per the native struct's own char-array semantics (ADR-194 Decision).
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct NdisAdapterInfo(pub NDIS_ADAPTER_INFORMATION);

impl NdisAdapterInfo {
    pub(crate) fn zeroed() -> Self {
        Self(unsafe { MaybeUninit::<NDIS_ADAPTER_INFORMATION>::zeroed().assume_init() })
    }

    pub(crate) fn as_raw_mut_ptr(&mut self) -> *mut NDIS_ADAPTER_INFORMATION {
        &mut self.0
    }

    /// The raw 128-byte `AdapterUniqueID` field, byte-for-byte (native
    /// `c_char` cast to `u8`; no C-string interpretation).
    pub fn adapter_unique_id(&self) -> [u8; 128] {
        self.0.AdapterUniqueID.map(|c| c as u8)
    }

    /// The raw 64-byte `AdapterName` field, byte-for-byte.
    pub fn adapter_name(&self) -> [u8; 64] {
        self.0.AdapterName.map(|c| c as u8)
    }

    /// The adapter's native `Status` field.
    pub fn status(&self) -> u32 {
        self.0.Status
    }

    /// The 6-byte MAC address, passed through byte-for-byte (already
    /// network-order per the spec).
    pub fn mac_address(&self) -> [u8; 6] {
        self.0.MAC_Address
    }

    /// The 16-byte IPv6 address, passed through byte-for-byte.
    pub fn ipv6_address(&self) -> [u8; 16] {
        self.0.IPV6_Address
    }

    /// The 4-byte IPv4 address, passed through byte-for-byte.
    pub fn ipv4_address(&self) -> [u8; 4] {
        self.0.IPV4_Address
    }

    /// The adapter's native `EthernetPinConfig` field.
    pub fn ethernet_pin_config(&self) -> u32 {
        self.0.EthernetPinConfig
    }
}

fn validate_data_size(size: usize) -> Result<usize, Error> {
    if size > MAX_MESSAGE_DATA {
        Err(Error::MessageTooLarge {
            len: size,
            max: MAX_MESSAGE_DATA,
        })
    } else {
        Ok(size)
    }
}
