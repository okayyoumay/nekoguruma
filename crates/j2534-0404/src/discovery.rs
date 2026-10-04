//! SAE J2534-2 clause 25 Discovery Mechanism types (`GET_DEVICE_INFO`/`GET_PROTOCOL_INFO`).

/// One `SPARAM` query to send to `GET_DEVICE_INFO`/`GET_PROTOCOL_INFO`
/// (clause 25.3.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryParam {
    /// The `DEVICE_INFO_*`/`PROTOCOL_INFO_*` parameter ID being queried.
    pub parameter: u32,
    /// Input value for the handful of parameters the device also reads as
    /// an application input (e.g. a pin-selection bitmask); ignored by the
    /// device for pure-output parameters.
    pub value: u32,
}

/// The device's answer to one [`DiscoveryParam`] query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryResult {
    /// Echoes the queried parameter ID.
    pub parameter: u32,
    /// The parameter's value; unaltered from the query's input `value` when
    /// `supported` is `false`, per clause 25.3.2.2's `SPARAM.Supported` note.
    pub value: u32,
    /// Whether the device supports this parameter (`SPARAM.Supported`).
    pub supported: bool,
}
