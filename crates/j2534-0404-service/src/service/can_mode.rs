//! CAN-bus channel operating mode selection (ADR-046).
//!
//! When a gRPC client uses a CAN-based protocol (raw CAN or an ISO 15765-2
//! stack), the way the service maps the ComLogicalLink onto J2534 physical
//! channels is selected per library via the `can_channel_mode` key in
//! `config.toml` (read through `vci-service-config`):
//!
//! ```toml
//! [config.apis.j2534-0404.libs."OpenPort2"]
//! can_channel_mode = "dual-channel"   # or "single-channel" / "software-isotp" / "auto" / "native-mixed" / "native-mixed-all-frames"
//! ```

use super::ChannelProtocol;

/// How CAN-family ComLogicalLinks are mapped onto J2534 physical channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CanChannelMode {
    /// One CLL may use both an ISO15765 J2534 channel (USDT) and a raw CAN
    /// J2534 channel (UUDT reception / raw frames) at the same time.  The
    /// companion CAN channel is opened on demand — when the CLL's
    /// UniqueRespIdTable configures a `CP_CanRespUUDTId` — and is shared
    /// (via `shared_channels` ref-counting) with any raw-CAN CLLs at the
    /// same baud rate.  Requires a J2534 library/device that supports an
    /// ISO15765 channel and a CAN channel open simultaneously.
    DualChannel,

    /// One CLL uses exactly one J2534 channel, selected by its protocol:
    /// ISO15765-family CLLs get an ISO15765 channel (USDT), raw-CAN CLLs get
    /// a CAN channel.  The functionality of the other channel type is not
    /// available from that CLL — a client that needs both must explicitly
    /// open a second CLL with the other protocol.  Whether two such CLLs can
    /// be connected at the same time depends on the J2534 library supporting
    /// two simultaneous channels.  This is the pre-ADR-046 behaviour and the
    /// default. (UUDT reception on an ISO15765 CLL still relies on the
    /// device honouring the UUDT `FLOW_CONTROL_FILTER` workaround, ADR-041.)
    #[default]
    SingleChannel,

    /// Always use a raw CAN J2534 channel for CAN-family CLLs.  USDT (ISO
    /// 15765-2 segmentation, flow control, padding) is performed in software
    /// by this service (`service/isotp.rs`) instead of by the J2534 library.
    /// Useful for J2534 libraries with absent or unreliable ISO15765 channel
    /// support; also gives UUDT and raw-frame access on the same channel for
    /// free.
    SoftwareIsoTp,

    /// Probes hardware capability instead of a fixed choice (ADR-046
    /// addendum): the first time an ISO15765-family CLL establishes a new
    /// physical `ISO15765` channel, the service additionally attempts to
    /// open a companion raw `CAN` channel at the same baud rate. If that
    /// succeeds, the device is treated as [`DualChannel`](Self::DualChannel)
    /// from then on; if it fails (e.g. `ERR_EXCEEDED_LIMIT` /
    /// `ERR_DEVICE_IN_USE`), the device is treated as
    /// [`SingleChannel`](Self::SingleChannel). The probe channel is closed
    /// immediately either way — this only decides policy, it does not keep
    /// a channel open. The decision is cached per open-device session
    /// (`J2534Service::resolved_can_channel_mode`, epoch-tagged against
    /// `device_epoch`), not for the service's whole lifetime: a module
    /// switch (device close/reopen) invalidates the cached entry and forces
    /// a fresh probe the next time it's needed, so only the first qualifying
    /// connect *per device session* pays the probe cost.
    ///
    /// `Auto` never resolves to [`SoftwareIsoTp`](Self::SoftwareIsoTp): a
    /// working `ISO15765` channel and a failure to open a second channel are
    /// easy to tell apart, but a *present-but-unreliable* `ISO15765`
    /// channel is not distinguishable from a healthy one by a connect-time
    /// probe, so that choice is left to explicit configuration.
    Auto,

    /// Enables SAE J2534-2 clause 8 ("Mixed Format Frames on a CAN Network")
    /// on an ISO15765-family CLL's primary channel instead of relying on the
    /// ADR-041 UUDT `FLOW_CONTROL_FILTER` workaround (ADR-160). The primary
    /// `ISO15765`/`ISO15765_PS` channel gets `SET_CONFIG(CAN_MIXED_FORMAT,
    /// ON)` once, at physical-channel creation — never per-CLL, never
    /// re-issued for a CLL that joins an already-open shared channel.
    /// Instead of the ADR-041 `FLOW_CONTROL_FILTER` fallback, a UUDT
    /// response id (`CP_CanRespUUDTId`) gets a genuine, address-narrowed
    /// `PASS_FILTER` installed on the same channel; the device's own native
    /// `ProtocolID` tagging on each received frame (ISO15765 vs. raw CAN)
    /// decides USDT-vs-UUDT routing per frame. `ERR_NOT_SUPPORTED` from the
    /// connect-time `SET_CONFIG` call fails the connect outright — no
    /// silent fallback to the ADR-041 workaround. Requires a device that
    /// actually implements clause 8; an `FD_ISO15765_PS`-substituted link
    /// (ADR-159) is excluded and keeps using the ADR-159 fallback instead
    /// (clause 8's raw-CAN-vs-CAN-FD frame format distinction is out of
    /// scope for this mode, ADR-160).
    NativeMixed,

    /// Enables SAE J2534-2 clause 8 ("Mixed Format Frames on a CAN Network")
    /// the same way [`NativeMixed`](Self::NativeMixed) does, except the
    /// primary `ISO15765`/`ISO15765_PS` channel gets
    /// `SET_CONFIG(CAN_MIXED_FORMAT, ALL_FRAMES)` (value `2`) instead of
    /// `ON` (value `1`) at physical-channel creation. Under `ALL_FRAMES`,
    /// clause 8.2.1's `FLOW_CONTROL_FILTER` and `PASS_FILTER`/`BLOCK_FILTER`
    /// evaluation happens independently and in parallel per frame rather
    /// than either/or, so a frame matching both a flow-control-eligible
    /// USDT filter and a UUDT `PASS_FILTER` is delivered twice — once
    /// reassembled as ISO15765, once as a raw CAN frame. This is the
    /// sub-mode under which the ADR-162 `UniqueRespIdTable`
    /// `CP_CanRespUUDTId`/`CP_CanRespUSDTId` collision check no longer
    /// applies: the collision `ON` must reject (the UUDT interpretation
    /// being unreachable) is exactly the case `ALL_FRAMES` makes servable,
    /// since both interpretations are independently deliverable (ADR-217
    /// Decision item 3).
    NativeMixedAllFrames,
}

impl CanChannelMode {
    /// Parses a `can_channel_mode` config string (case-insensitive; `_` and
    /// `-` are interchangeable).  Returns `Err` with the offending value so
    /// the service can fail fast at startup on a typo instead of silently
    /// running in the wrong mode.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "dual-channel" | "dual" => Ok(Self::DualChannel),
            "single-channel" | "single" => Ok(Self::SingleChannel),
            "software-isotp" | "software-iso-tp" | "soft-isotp" => Ok(Self::SoftwareIsoTp),
            "auto" => Ok(Self::Auto),
            "native-mixed" | "native-mix" | "mixed-format" | "mixed" => Ok(Self::NativeMixed),
            "native-mixed-all-frames" | "native-mixed-all" | "mixed-format-all" | "mixed-all" => {
                Ok(Self::NativeMixedAllFrames)
            }
            other => Err(format!(
                "invalid can_channel_mode {other:?}; expected \"dual-channel\", \
                 \"single-channel\", \"software-isotp\", \"auto\", \"native-mixed\", \
                 or \"native-mixed-all-frames\""
            )),
        }
    }

    /// Loads the mode for `library_name` from `config.toml` via
    /// `vci-service-config`, defaulting to [`CanChannelMode::SingleChannel`]
    /// when the key is absent.
    pub fn from_config(arch: Option<&str>, library_name: &str) -> Result<Self, String> {
        match vci_service_launcher::config::find_can_channel_mode("j2534-0404", arch, library_name)
        {
            Some(value) => Self::parse(&value),
            None => Ok(Self::default()),
        }
    }

    /// Returns `true` when `protocol` runs an ISO 15765-2 transport over the
    /// CAN bus (i.e. `j2534_protocol_id()` is ISO15765) — the protocols whose
    /// channel mapping this mode controls.
    pub fn applies_to(protocol: ChannelProtocol) -> bool {
        protocol.j2534_protocol_id() == j2534_0404::ISO15765
    }

    /// The J2534 hardware protocol ID actually passed to `PassThruConnect`
    /// (and used for `ChannelKey`, `PassThruMessage`, filters, and ADR-028
    /// SET_CONFIG gating) for a CLL with service-level protocol `protocol`.
    ///
    /// In `SoftwareIsoTp` mode ISO15765-family CLLs connect a raw CAN
    /// channel; every other combination passes through unchanged. `Auto`
    /// always passes through unchanged too — its `DualChannel` vs.
    /// `SingleChannel` sub-decision does not affect which hardware protocol
    /// an ISO15765-family CLL's *primary* channel connects with, only
    /// whether a companion channel is also opened.
    pub fn hw_protocol_id(self, protocol: ChannelProtocol) -> u32 {
        if self == Self::SoftwareIsoTp && Self::applies_to(protocol) {
            j2534_0404::CAN
        } else {
            protocol.j2534_protocol_id()
        }
    }

    /// Returns `true` when a CLL with `protocol` performs ISO-TP in software
    /// under this mode (TX segmentation and RX reassembly in the service).
    pub fn is_software_isotp(self, protocol: ChannelProtocol) -> bool {
        self == Self::SoftwareIsoTp && Self::applies_to(protocol)
    }

    /// Returns `true` for either native-mixed sub-mode
    /// ([`NativeMixed`](Self::NativeMixed) or
    /// [`NativeMixedAllFrames`](Self::NativeMixedAllFrames)). Use this at
    /// family-wide call sites where `ON` and `ALL_FRAMES` behave
    /// identically: whether SAE J2534-2 clause 8 applies to a connect at
    /// all (SET_CONFIG applicability), UUDT `PASS_FILTER` installation, and
    /// the RX per-frame native `ProtocolID` branch gate. Do NOT use this
    /// for the ADR-162 `UniqueRespIdTable` collision checks — those remain
    /// `ON`-only and must stay a literal `== CanChannelMode::NativeMixed`
    /// comparison (ADR-217 Decision item 3).
    pub fn is_native_mixed_family(&self) -> bool {
        matches!(self, Self::NativeMixed | Self::NativeMixedAllFrames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_mode_spellings() {
        assert_eq!(
            CanChannelMode::parse("dual-channel"),
            Ok(CanChannelMode::DualChannel)
        );
        assert_eq!(
            CanChannelMode::parse("Dual_Channel"),
            Ok(CanChannelMode::DualChannel)
        );
        assert_eq!(
            CanChannelMode::parse("single-channel"),
            Ok(CanChannelMode::SingleChannel)
        );
        assert_eq!(
            CanChannelMode::parse(" single "),
            Ok(CanChannelMode::SingleChannel)
        );
        assert_eq!(
            CanChannelMode::parse("software-isotp"),
            Ok(CanChannelMode::SoftwareIsoTp)
        );
        assert_eq!(
            CanChannelMode::parse("SOFTWARE_ISO_TP"),
            Ok(CanChannelMode::SoftwareIsoTp)
        );
        assert_eq!(CanChannelMode::parse("auto"), Ok(CanChannelMode::Auto));
        assert_eq!(CanChannelMode::parse(" Auto "), Ok(CanChannelMode::Auto));
        assert_eq!(
            CanChannelMode::parse("native-mixed"),
            Ok(CanChannelMode::NativeMixed)
        );
        assert_eq!(
            CanChannelMode::parse("Native_Mixed"),
            Ok(CanChannelMode::NativeMixed)
        );
        assert_eq!(
            CanChannelMode::parse("native-mix"),
            Ok(CanChannelMode::NativeMixed)
        );
        assert_eq!(
            CanChannelMode::parse("mixed-format"),
            Ok(CanChannelMode::NativeMixed)
        );
        assert_eq!(
            CanChannelMode::parse("mixed"),
            Ok(CanChannelMode::NativeMixed)
        );
        assert_eq!(
            CanChannelMode::parse("native-mixed-all-frames"),
            Ok(CanChannelMode::NativeMixedAllFrames)
        );
        assert_eq!(
            CanChannelMode::parse("Native_Mixed_All_Frames"),
            Ok(CanChannelMode::NativeMixedAllFrames)
        );
        assert_eq!(
            CanChannelMode::parse("native-mixed-all"),
            Ok(CanChannelMode::NativeMixedAllFrames)
        );
        assert_eq!(
            CanChannelMode::parse("mixed-format-all"),
            Ok(CanChannelMode::NativeMixedAllFrames)
        );
        assert_eq!(
            CanChannelMode::parse("mixed-all"),
            Ok(CanChannelMode::NativeMixedAllFrames)
        );
    }

    #[test]
    fn rejects_unknown_mode() {
        let err = CanChannelMode::parse("triple-channel").expect_err("must be rejected");
        assert!(err.contains("triple-channel"));
    }

    #[test]
    fn default_is_single_channel() {
        assert_eq!(CanChannelMode::default(), CanChannelMode::SingleChannel);
    }

    #[test]
    fn hw_protocol_id_maps_iso15765_to_can_only_in_software_mode() {
        let iso = ChannelProtocol::ISO15765;
        let ext = ChannelProtocol::ISO_14229_3_ON_ISO_15765_2;
        let raw_can = ChannelProtocol::CAN;
        let kwp = ChannelProtocol::ISO14230;

        assert_eq!(
            CanChannelMode::SoftwareIsoTp.hw_protocol_id(iso),
            j2534_0404::CAN
        );
        assert_eq!(
            CanChannelMode::SoftwareIsoTp.hw_protocol_id(ext),
            j2534_0404::CAN
        );
        assert_eq!(
            CanChannelMode::SoftwareIsoTp.hw_protocol_id(raw_can),
            j2534_0404::CAN
        );
        assert_eq!(
            CanChannelMode::SoftwareIsoTp.hw_protocol_id(kwp),
            j2534_0404::ISO14230
        );

        assert_eq!(
            CanChannelMode::SingleChannel.hw_protocol_id(iso),
            j2534_0404::ISO15765
        );
        assert_eq!(
            CanChannelMode::DualChannel.hw_protocol_id(iso),
            j2534_0404::ISO15765
        );
    }

    #[test]
    fn software_isotp_applies_only_to_iso15765_family() {
        assert!(CanChannelMode::SoftwareIsoTp.is_software_isotp(ChannelProtocol::ISO15765));
        assert!(
            CanChannelMode::SoftwareIsoTp
                .is_software_isotp(ChannelProtocol::ISO_15031_5_ON_ISO_15765_4)
        );
        assert!(!CanChannelMode::SoftwareIsoTp.is_software_isotp(ChannelProtocol::CAN));
        assert!(!CanChannelMode::SingleChannel.is_software_isotp(ChannelProtocol::ISO15765));
    }

    #[test]
    fn auto_never_selects_software_isotp() {
        // Auto's hw_protocol_id/is_software_isotp behave like SingleChannel:
        // its DualChannel-vs-SingleChannel sub-decision only affects whether
        // a companion channel is opened, resolved separately at runtime.
        let iso = ChannelProtocol::ISO15765;
        assert_eq!(
            CanChannelMode::Auto.hw_protocol_id(iso),
            j2534_0404::ISO15765
        );
        assert!(!CanChannelMode::Auto.is_software_isotp(iso));
    }

    #[test]
    fn native_mixed_passes_through_hw_protocol_id_and_is_never_software_isotp() {
        // NativeMixed's SET_CONFIG(CAN_MIXED_FORMAT) step only affects the
        // primary channel's connect-time configuration, not which hardware
        // protocol it connects with — it behaves like SingleChannel here,
        // mirroring Auto's own pass-through shape.
        let iso = ChannelProtocol::ISO15765;
        assert_eq!(
            CanChannelMode::NativeMixed.hw_protocol_id(iso),
            j2534_0404::ISO15765
        );
        assert!(!CanChannelMode::NativeMixed.is_software_isotp(iso));
    }

    #[test]
    fn native_mixed_all_frames_passes_through_hw_protocol_id_and_is_never_software_isotp() {
        // Mirrors NativeMixed's own pass-through shape (ADR-217): the
        // ALL_FRAMES sub-mode only affects the primary channel's
        // connect-time SET_CONFIG value, not which hardware protocol it
        // connects with.
        let iso = ChannelProtocol::ISO15765;
        assert_eq!(
            CanChannelMode::NativeMixedAllFrames.hw_protocol_id(iso),
            j2534_0404::ISO15765
        );
        assert!(!CanChannelMode::NativeMixedAllFrames.is_software_isotp(iso));
    }

    #[test]
    fn is_native_mixed_family_covers_both_sub_modes_only() {
        assert!(CanChannelMode::NativeMixed.is_native_mixed_family());
        assert!(CanChannelMode::NativeMixedAllFrames.is_native_mixed_family());
        assert!(!CanChannelMode::SingleChannel.is_native_mixed_family());
        assert!(!CanChannelMode::DualChannel.is_native_mixed_family());
        assert!(!CanChannelMode::SoftwareIsoTp.is_native_mixed_family());
        assert!(!CanChannelMode::Auto.is_native_mixed_family());
    }
}
