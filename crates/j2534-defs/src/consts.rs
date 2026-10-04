//! J2534 v04.04 numeric constants.
//!
//! Values match `crates/j2534-0404-sys/src/bindings/j2534_v0404.h`.
//!
//! All values fit in 32 bits, so they are declared as `u32` regardless of the
//! width of `unsigned long` in a given vendor library (7.1.2). The worker
//! widens them to the ABI width at the call site.

/// Return codes of every `PassThru*` function (J2534-1 section 7.2, plus the additional codes defined by J2534-2).
pub mod status {
    pub const STATUS_NOERROR: u32 = 0x00000000;
    pub const ERR_NOT_SUPPORTED: u32 = 0x00000001;
    pub const ERR_INVALID_CHANNEL_ID: u32 = 0x00000002;
    pub const ERR_INVALID_PROTOCOL_ID: u32 = 0x00000003;
    pub const ERR_NULL_PARAMETER: u32 = 0x00000004;
    pub const ERR_INVALID_IOCTL_VALUE: u32 = 0x00000005;
    pub const ERR_INVALID_FLAGS: u32 = 0x00000006;
    pub const ERR_FAILED: u32 = 0x00000007;
    pub const ERR_DEVICE_NOT_CONNECTED: u32 = 0x00000008;
    pub const ERR_TIMEOUT: u32 = 0x00000009;
    pub const ERR_INVALID_MSG: u32 = 0x0000000A;
    pub const ERR_INVALID_TIME_INTERVAL: u32 = 0x0000000B;
    pub const ERR_EXCEEDED_LIMIT: u32 = 0x0000000C;
    pub const ERR_INVALID_MSG_ID: u32 = 0x0000000D;
    pub const ERR_DEVICE_IN_USE: u32 = 0x0000000E;
    pub const ERR_INVALID_IOCTL_ID: u32 = 0x0000000F;
    pub const ERR_BUFFER_EMPTY: u32 = 0x00000010;
    pub const ERR_BUFFER_FULL: u32 = 0x00000011;
    pub const ERR_BUFFER_OVERFLOW: u32 = 0x00000012;
    pub const ERR_PIN_INVALID: u32 = 0x00000013;
    pub const ERR_CHANNEL_IN_USE: u32 = 0x00000014;
    pub const ERR_MSG_PROTOCOL_ID: u32 = 0x00000015;
    pub const ERR_INVALID_FILTER_ID: u32 = 0x00000016;
    pub const ERR_NO_FLOW_CONTROL: u32 = 0x00000017;
    pub const ERR_NOT_UNIQUE: u32 = 0x00000018;
    pub const ERR_INVALID_BAUDRATE: u32 = 0x00000019;
    pub const ERR_INVALID_DEVICE_ID: u32 = 0x0000001A;
    /// J2534-2.
    pub const ERR_PIN_IN_USE: u32 = 0x00000020;
    /// J2534-2.
    pub const ERR_VOLTAGE_IN_USE: u32 = 0x0000001F;
    /// J2534-2.
    pub const ERR_ADDRESS_NOT_CLAIMED: u32 = 0x00010000;
    /// J2534-2.
    pub const ERR_NO_CONNECTION_ESTABLISHED: u32 = 0x00010001;
    /// J2534-2.
    pub const ERR_RESOURCE_IN_USE: u32 = 0x00010002;
    /// J2534-2.
    pub const ERR_INVALID_IOCTL_PARAM_ID: u32 = 0x0000001E;

    /// Symbolic name of a value in this group, if known.
    pub fn name(value: u32) -> Option<&'static str> {
        Some(match value {
            STATUS_NOERROR => "STATUS_NOERROR",
            ERR_NOT_SUPPORTED => "ERR_NOT_SUPPORTED",
            ERR_INVALID_CHANNEL_ID => "ERR_INVALID_CHANNEL_ID",
            ERR_INVALID_PROTOCOL_ID => "ERR_INVALID_PROTOCOL_ID",
            ERR_NULL_PARAMETER => "ERR_NULL_PARAMETER",
            ERR_INVALID_IOCTL_VALUE => "ERR_INVALID_IOCTL_VALUE",
            ERR_INVALID_FLAGS => "ERR_INVALID_FLAGS",
            ERR_FAILED => "ERR_FAILED",
            ERR_DEVICE_NOT_CONNECTED => "ERR_DEVICE_NOT_CONNECTED",
            ERR_TIMEOUT => "ERR_TIMEOUT",
            ERR_INVALID_MSG => "ERR_INVALID_MSG",
            ERR_INVALID_TIME_INTERVAL => "ERR_INVALID_TIME_INTERVAL",
            ERR_EXCEEDED_LIMIT => "ERR_EXCEEDED_LIMIT",
            ERR_INVALID_MSG_ID => "ERR_INVALID_MSG_ID",
            ERR_DEVICE_IN_USE => "ERR_DEVICE_IN_USE",
            ERR_INVALID_IOCTL_ID => "ERR_INVALID_IOCTL_ID",
            ERR_BUFFER_EMPTY => "ERR_BUFFER_EMPTY",
            ERR_BUFFER_FULL => "ERR_BUFFER_FULL",
            ERR_BUFFER_OVERFLOW => "ERR_BUFFER_OVERFLOW",
            ERR_PIN_INVALID => "ERR_PIN_INVALID",
            ERR_CHANNEL_IN_USE => "ERR_CHANNEL_IN_USE",
            ERR_MSG_PROTOCOL_ID => "ERR_MSG_PROTOCOL_ID",
            ERR_INVALID_FILTER_ID => "ERR_INVALID_FILTER_ID",
            ERR_NO_FLOW_CONTROL => "ERR_NO_FLOW_CONTROL",
            ERR_NOT_UNIQUE => "ERR_NOT_UNIQUE",
            ERR_INVALID_BAUDRATE => "ERR_INVALID_BAUDRATE",
            ERR_INVALID_DEVICE_ID => "ERR_INVALID_DEVICE_ID",
            ERR_PIN_IN_USE => "ERR_PIN_IN_USE",
            ERR_VOLTAGE_IN_USE => "ERR_VOLTAGE_IN_USE",
            ERR_ADDRESS_NOT_CLAIMED => "ERR_ADDRESS_NOT_CLAIMED",
            ERR_NO_CONNECTION_ESTABLISHED => "ERR_NO_CONNECTION_ESTABLISHED",
            ERR_RESOURCE_IN_USE => "ERR_RESOURCE_IN_USE",
            ERR_INVALID_IOCTL_PARAM_ID => "ERR_INVALID_IOCTL_PARAM_ID",
            _ => return None,
        })
    }
}

/// `ProtocolID` values for `PassThruConnect` (J2534-1 base protocols, plus the J2534-2 pin-switched `_PS` variants). The per-channel `_CHx` and analog-input variants of J2534-2 are not listed.
pub mod protocol {
    pub const PROTOCOL_J1850VPW: u32 = 0x00000001;
    pub const PROTOCOL_J1850PWM: u32 = 0x00000002;
    pub const PROTOCOL_ISO9141: u32 = 0x00000003;
    pub const PROTOCOL_ISO14230: u32 = 0x00000004;
    pub const PROTOCOL_CAN: u32 = 0x00000005;
    pub const PROTOCOL_ISO15765: u32 = 0x00000006;
    pub const PROTOCOL_SCI_A_ENGINE: u32 = 0x00000007;
    pub const PROTOCOL_SCI_A_TRANS: u32 = 0x00000008;
    pub const PROTOCOL_SCI_B_ENGINE: u32 = 0x00000009;
    pub const PROTOCOL_SCI_B_TRANS: u32 = 0x0000000A;
    /// J2534-2.
    pub const PROTOCOL_J1850VPW_PS: u32 = 0x00008000;
    /// J2534-2.
    pub const PROTOCOL_J1850PWM_PS: u32 = 0x00008001;
    /// J2534-2.
    pub const PROTOCOL_ISO9141_PS: u32 = 0x00008002;
    /// J2534-2.
    pub const PROTOCOL_ISO14230_PS: u32 = 0x00008003;
    /// J2534-2.
    pub const PROTOCOL_CAN_PS: u32 = 0x00008004;
    /// J2534-2.
    pub const PROTOCOL_ISO15765_PS: u32 = 0x00008005;
    /// J2534-2.
    pub const PROTOCOL_J2610_PS: u32 = 0x00008006;
    /// J2534-2.
    pub const PROTOCOL_SW_ISO15765_PS: u32 = 0x00008007;
    /// J2534-2.
    pub const PROTOCOL_SW_CAN_PS: u32 = 0x00008008;
    /// J2534-2.
    pub const PROTOCOL_GM_UART_PS: u32 = 0x00008009;
    /// J2534-2.
    pub const PROTOCOL_UART_ECHO_BYTE_PS: u32 = 0x0000800A;
    /// J2534-2.
    pub const PROTOCOL_HONDA_DIAGH_PS: u32 = 0x0000800B;
    /// J2534-2.
    pub const PROTOCOL_J1939_PS: u32 = 0x0000800C;
    /// J2534-2.
    pub const PROTOCOL_J1708_PS: u32 = 0x0000800D;
    /// J2534-2.
    pub const PROTOCOL_TP2_0_PS: u32 = 0x0000800E;
    /// J2534-2.
    pub const PROTOCOL_FT_CAN_PS: u32 = 0x0000800F;
    /// J2534-2.
    pub const PROTOCOL_FT_ISO15765_PS: u32 = 0x00008010;
    /// J2534-2.
    pub const PROTOCOL_FD_CAN_PS: u32 = 0x00008011;
    /// J2534-2.
    pub const PROTOCOL_FD_ISO15765_PS: u32 = 0x00008012;
    /// J2534-2.
    pub const PROTOCOL_ETHERNET_NDIS: u32 = 0x00008013;

    /// Symbolic name of a value in this group, if known.
    pub fn name(value: u32) -> Option<&'static str> {
        Some(match value {
            PROTOCOL_J1850VPW => "PROTOCOL_J1850VPW",
            PROTOCOL_J1850PWM => "PROTOCOL_J1850PWM",
            PROTOCOL_ISO9141 => "PROTOCOL_ISO9141",
            PROTOCOL_ISO14230 => "PROTOCOL_ISO14230",
            PROTOCOL_CAN => "PROTOCOL_CAN",
            PROTOCOL_ISO15765 => "PROTOCOL_ISO15765",
            PROTOCOL_SCI_A_ENGINE => "PROTOCOL_SCI_A_ENGINE",
            PROTOCOL_SCI_A_TRANS => "PROTOCOL_SCI_A_TRANS",
            PROTOCOL_SCI_B_ENGINE => "PROTOCOL_SCI_B_ENGINE",
            PROTOCOL_SCI_B_TRANS => "PROTOCOL_SCI_B_TRANS",
            PROTOCOL_J1850VPW_PS => "PROTOCOL_J1850VPW_PS",
            PROTOCOL_J1850PWM_PS => "PROTOCOL_J1850PWM_PS",
            PROTOCOL_ISO9141_PS => "PROTOCOL_ISO9141_PS",
            PROTOCOL_ISO14230_PS => "PROTOCOL_ISO14230_PS",
            PROTOCOL_CAN_PS => "PROTOCOL_CAN_PS",
            PROTOCOL_ISO15765_PS => "PROTOCOL_ISO15765_PS",
            PROTOCOL_J2610_PS => "PROTOCOL_J2610_PS",
            PROTOCOL_SW_ISO15765_PS => "PROTOCOL_SW_ISO15765_PS",
            PROTOCOL_SW_CAN_PS => "PROTOCOL_SW_CAN_PS",
            PROTOCOL_GM_UART_PS => "PROTOCOL_GM_UART_PS",
            PROTOCOL_UART_ECHO_BYTE_PS => "PROTOCOL_UART_ECHO_BYTE_PS",
            PROTOCOL_HONDA_DIAGH_PS => "PROTOCOL_HONDA_DIAGH_PS",
            PROTOCOL_J1939_PS => "PROTOCOL_J1939_PS",
            PROTOCOL_J1708_PS => "PROTOCOL_J1708_PS",
            PROTOCOL_TP2_0_PS => "PROTOCOL_TP2_0_PS",
            PROTOCOL_FT_CAN_PS => "PROTOCOL_FT_CAN_PS",
            PROTOCOL_FT_ISO15765_PS => "PROTOCOL_FT_ISO15765_PS",
            PROTOCOL_FD_CAN_PS => "PROTOCOL_FD_CAN_PS",
            PROTOCOL_FD_ISO15765_PS => "PROTOCOL_FD_ISO15765_PS",
            PROTOCOL_ETHERNET_NDIS => "PROTOCOL_ETHERNET_NDIS",
            _ => return None,
        })
    }
}

/// `Flags` bits for `PassThruConnect` (J2534-1).
pub mod connect_flag {
    pub const CONNECT_FLAG_ISO9141_K_LINE_ONLY: u32 = 0x00001000;
    pub const CONNECT_FLAG_CAN_ID_BOTH: u32 = 0x00000800;
    pub const CONNECT_FLAG_ISO9141_NO_CHECKSUM: u32 = 0x00000200;
    pub const CONNECT_FLAG_CAN_29BIT_ID: u32 = 0x00000100;
}

/// `TxFlags` bits of `PASSTHRU_MSG` (J2534-1).
pub mod tx_flag {
    pub const TX_FLAG_SCI_TX_VOLTAGE: u32 = 0x00800000;
    pub const TX_FLAG_SCI_MODE: u32 = 0x00400000;
    pub const TX_FLAG_WAIT_P3_MIN_ONLY: u32 = 0x00000200;
    pub const TX_FLAG_CAN_29BIT_ID: u32 = 0x00000100;
    pub const TX_FLAG_ISO15765_ADDR_TYPE: u32 = 0x00000080;
    pub const TX_FLAG_ISO15765_FRAME_PAD: u32 = 0x00000040;
}

/// `RxStatus` bits of `PASSTHRU_MSG` (J2534-1).
pub mod rx_flag {
    pub const RX_FLAG_CAN_29BIT_ID: u32 = 0x00000100;
    pub const RX_FLAG_ISO15765_ADDR_TYPE: u32 = 0x00000080;
    pub const RX_FLAG_ISO15765_PADDING_ERROR: u32 = 0x00000010;
    pub const RX_FLAG_TX_INDICATION: u32 = 0x00000008;
    pub const RX_FLAG_BREAK: u32 = 0x00000004;
    pub const RX_FLAG_START_OF_MESSAGE: u32 = 0x00000002;
    pub const RX_TX_MSG_TYPE: u32 = 0x00000001;
}

/// `FilterType` values for `PassThruStartMsgFilter` (J2534-1).
pub mod filter {
    pub const PASS_FILTER: u32 = 0x00000001;
    pub const BLOCK_FILTER: u32 = 0x00000002;
    pub const FLOW_CONTROL_FILTER: u32 = 0x00000003;
}

/// `SCONFIG.Parameter` IDs for the `GET_CONFIG` / `SET_CONFIG` IOCTLs (J2534-1).
pub mod config {
    pub const CONFIG_DATA_RATE: u32 = 0x00000001;
    pub const CONFIG_LOOPBACK: u32 = 0x00000003;
    pub const CONFIG_NODE_ADDRESS: u32 = 0x00000004;
    pub const CONFIG_NETWORK_LINE: u32 = 0x00000005;
    pub const CONFIG_P1_MIN: u32 = 0x00000006;
    pub const CONFIG_P1_MAX: u32 = 0x00000007;
    pub const CONFIG_P2_MIN: u32 = 0x00000008;
    pub const CONFIG_P2_MAX: u32 = 0x00000009;
    pub const CONFIG_P3_MIN: u32 = 0x0000000A;
    pub const CONFIG_P3_MAX: u32 = 0x0000000B;
    pub const CONFIG_P4_MIN: u32 = 0x0000000C;
    pub const CONFIG_P4_MAX: u32 = 0x0000000D;
    pub const CONFIG_W0: u32 = 0x00000019;
    pub const CONFIG_W1: u32 = 0x0000000E;
    pub const CONFIG_W2: u32 = 0x0000000F;
    pub const CONFIG_W3: u32 = 0x00000010;
    pub const CONFIG_W4: u32 = 0x00000011;
    pub const CONFIG_W5: u32 = 0x00000012;
    pub const CONFIG_TIDLE: u32 = 0x00000013;
    pub const CONFIG_TINIL: u32 = 0x00000014;
    pub const CONFIG_TWUP: u32 = 0x00000015;
    pub const CONFIG_PARITY: u32 = 0x00000016;
    pub const CONFIG_BIT_SAMPLE_POINT: u32 = 0x00000017;
    pub const CONFIG_SYNC_JUMP_WIDTH: u32 = 0x00000018;
    pub const CONFIG_T1_MAX: u32 = 0x0000001A;
    pub const CONFIG_T2_MAX: u32 = 0x0000001B;
    pub const CONFIG_T3_MAX: u32 = 0x00000024;
    pub const CONFIG_T4_MAX: u32 = 0x0000001C;
    pub const CONFIG_T5_MAX: u32 = 0x0000001D;
    pub const CONFIG_ISO15765_BS: u32 = 0x0000001E;
    pub const CONFIG_ISO15765_STMIN: u32 = 0x0000001F;
    pub const CONFIG_DATA_BITS: u32 = 0x00000020;
    pub const CONFIG_FIVE_BAUD_MOD: u32 = 0x00000021;
    pub const CONFIG_BS_TX: u32 = 0x00000022;
    pub const CONFIG_STMIN_TX: u32 = 0x00000023;
    pub const CONFIG_ISO15765_WFT_MAX: u32 = 0x00000025;

    /// Symbolic name of a value in this group, if known.
    pub fn name(value: u32) -> Option<&'static str> {
        Some(match value {
            CONFIG_DATA_RATE => "CONFIG_DATA_RATE",
            CONFIG_LOOPBACK => "CONFIG_LOOPBACK",
            CONFIG_NODE_ADDRESS => "CONFIG_NODE_ADDRESS",
            CONFIG_NETWORK_LINE => "CONFIG_NETWORK_LINE",
            CONFIG_P1_MIN => "CONFIG_P1_MIN",
            CONFIG_P1_MAX => "CONFIG_P1_MAX",
            CONFIG_P2_MIN => "CONFIG_P2_MIN",
            CONFIG_P2_MAX => "CONFIG_P2_MAX",
            CONFIG_P3_MIN => "CONFIG_P3_MIN",
            CONFIG_P3_MAX => "CONFIG_P3_MAX",
            CONFIG_P4_MIN => "CONFIG_P4_MIN",
            CONFIG_P4_MAX => "CONFIG_P4_MAX",
            CONFIG_W0 => "CONFIG_W0",
            CONFIG_W1 => "CONFIG_W1",
            CONFIG_W2 => "CONFIG_W2",
            CONFIG_W3 => "CONFIG_W3",
            CONFIG_W4 => "CONFIG_W4",
            CONFIG_W5 => "CONFIG_W5",
            CONFIG_TIDLE => "CONFIG_TIDLE",
            CONFIG_TINIL => "CONFIG_TINIL",
            CONFIG_TWUP => "CONFIG_TWUP",
            CONFIG_PARITY => "CONFIG_PARITY",
            CONFIG_BIT_SAMPLE_POINT => "CONFIG_BIT_SAMPLE_POINT",
            CONFIG_SYNC_JUMP_WIDTH => "CONFIG_SYNC_JUMP_WIDTH",
            CONFIG_T1_MAX => "CONFIG_T1_MAX",
            CONFIG_T2_MAX => "CONFIG_T2_MAX",
            CONFIG_T3_MAX => "CONFIG_T3_MAX",
            CONFIG_T4_MAX => "CONFIG_T4_MAX",
            CONFIG_T5_MAX => "CONFIG_T5_MAX",
            CONFIG_ISO15765_BS => "CONFIG_ISO15765_BS",
            CONFIG_ISO15765_STMIN => "CONFIG_ISO15765_STMIN",
            CONFIG_DATA_BITS => "CONFIG_DATA_BITS",
            CONFIG_FIVE_BAUD_MOD => "CONFIG_FIVE_BAUD_MOD",
            CONFIG_BS_TX => "CONFIG_BS_TX",
            CONFIG_STMIN_TX => "CONFIG_STMIN_TX",
            CONFIG_ISO15765_WFT_MAX => "CONFIG_ISO15765_WFT_MAX",
            _ => return None,
        })
    }
}

/// `IoctlID` values for `PassThruIoctl` (J2534-1, plus the IOCTLs defined by J2534-2).
pub mod ioctl {
    pub const IOCTL_GET_CONFIG: u32 = 0x00000001;
    pub const IOCTL_SET_CONFIG: u32 = 0x00000002;
    pub const IOCTL_READ_VBATT: u32 = 0x00000003;
    pub const IOCTL_FIVE_BAUD_INIT: u32 = 0x00000004;
    pub const IOCTL_FAST_INIT: u32 = 0x00000005;
    pub const IOCTL_CLEAR_TX_BUFFER: u32 = 0x00000007;
    pub const IOCTL_CLEAR_RX_BUFFER: u32 = 0x00000008;
    pub const IOCTL_CLEAR_PERIODIC_MSGS: u32 = 0x00000009;
    pub const IOCTL_CLEAR_MSG_FILTERS: u32 = 0x0000000A;
    pub const IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE: u32 = 0x0000000B;
    pub const IOCTL_ADD_TO_FUNCT_MSG_LOOKUP_TABLE: u32 = 0x0000000C;
    pub const IOCTL_DELETE_FROM_FUNCT_MSG_LOOKUP_TABLE: u32 = 0x0000000D;
    pub const IOCTL_READ_PROG_VOLTAGE: u32 = 0x0000000E;
    /// J2534-2.
    pub const IOCTL_SW_CAN_HS: u32 = 0x00008000;
    /// J2534-2.
    pub const IOCTL_SW_CAN_NS: u32 = 0x00008001;
    /// J2534-2.
    pub const IOCTL_SET_POLL_RESPONSE: u32 = 0x00008002;
    /// J2534-2.
    pub const IOCTL_BECOME_MASTER: u32 = 0x00008003;
    /// J2534-2.
    pub const IOCTL_START_REPEAT_MESSAGE: u32 = 0x00008004;
    /// J2534-2.
    pub const IOCTL_QUERY_REPEAT_MESSAGE: u32 = 0x00008005;
    /// J2534-2.
    pub const IOCTL_STOP_REPEAT_MESSAGE: u32 = 0x00008006;
    /// J2534-2.
    pub const IOCTL_GET_DEVICE_CONFIG: u32 = 0x00008007;
    /// J2534-2.
    pub const IOCTL_SET_DEVICE_CONFIG: u32 = 0x00008008;
    /// J2534-2.
    pub const IOCTL_PROTECT_J1939_ADDR: u32 = 0x00008009;
    /// J2534-2.
    pub const IOCTL_REQUEST_CONNECTION: u32 = 0x0000800A;
    /// J2534-2.
    pub const IOCTL_TEARDOWN_CONNECTION: u32 = 0x0000800B;
    /// J2534-2.
    pub const IOCTL_GET_DEVICE_INFO: u32 = 0x0000800C;
    /// J2534-2.
    pub const IOCTL_GET_PROTOCOL_INFO: u32 = 0x0000800D;
    /// J2534-2.
    pub const IOCTL_READ_J1962PIN_VOLTAGE: u32 = 0x0000800E;
    /// J2534-2.
    pub const IOCTL_GET_NDIS_ADAPTER_INFO: u32 = 0x0000800F;

    /// Symbolic name of a value in this group, if known.
    pub fn name(value: u32) -> Option<&'static str> {
        Some(match value {
            IOCTL_GET_CONFIG => "IOCTL_GET_CONFIG",
            IOCTL_SET_CONFIG => "IOCTL_SET_CONFIG",
            IOCTL_READ_VBATT => "IOCTL_READ_VBATT",
            IOCTL_FIVE_BAUD_INIT => "IOCTL_FIVE_BAUD_INIT",
            IOCTL_FAST_INIT => "IOCTL_FAST_INIT",
            IOCTL_CLEAR_TX_BUFFER => "IOCTL_CLEAR_TX_BUFFER",
            IOCTL_CLEAR_RX_BUFFER => "IOCTL_CLEAR_RX_BUFFER",
            IOCTL_CLEAR_PERIODIC_MSGS => "IOCTL_CLEAR_PERIODIC_MSGS",
            IOCTL_CLEAR_MSG_FILTERS => "IOCTL_CLEAR_MSG_FILTERS",
            IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE => "IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE",
            IOCTL_ADD_TO_FUNCT_MSG_LOOKUP_TABLE => "IOCTL_ADD_TO_FUNCT_MSG_LOOKUP_TABLE",
            IOCTL_DELETE_FROM_FUNCT_MSG_LOOKUP_TABLE => "IOCTL_DELETE_FROM_FUNCT_MSG_LOOKUP_TABLE",
            IOCTL_READ_PROG_VOLTAGE => "IOCTL_READ_PROG_VOLTAGE",
            IOCTL_SW_CAN_HS => "IOCTL_SW_CAN_HS",
            IOCTL_SW_CAN_NS => "IOCTL_SW_CAN_NS",
            IOCTL_SET_POLL_RESPONSE => "IOCTL_SET_POLL_RESPONSE",
            IOCTL_BECOME_MASTER => "IOCTL_BECOME_MASTER",
            IOCTL_START_REPEAT_MESSAGE => "IOCTL_START_REPEAT_MESSAGE",
            IOCTL_QUERY_REPEAT_MESSAGE => "IOCTL_QUERY_REPEAT_MESSAGE",
            IOCTL_STOP_REPEAT_MESSAGE => "IOCTL_STOP_REPEAT_MESSAGE",
            IOCTL_GET_DEVICE_CONFIG => "IOCTL_GET_DEVICE_CONFIG",
            IOCTL_SET_DEVICE_CONFIG => "IOCTL_SET_DEVICE_CONFIG",
            IOCTL_PROTECT_J1939_ADDR => "IOCTL_PROTECT_J1939_ADDR",
            IOCTL_REQUEST_CONNECTION => "IOCTL_REQUEST_CONNECTION",
            IOCTL_TEARDOWN_CONNECTION => "IOCTL_TEARDOWN_CONNECTION",
            IOCTL_GET_DEVICE_INFO => "IOCTL_GET_DEVICE_INFO",
            IOCTL_GET_PROTOCOL_INFO => "IOCTL_GET_PROTOCOL_INFO",
            IOCTL_READ_J1962PIN_VOLTAGE => "IOCTL_READ_J1962PIN_VOLTAGE",
            IOCTL_GET_NDIS_ADAPTER_INFO => "IOCTL_GET_NDIS_ADAPTER_INFO",
            _ => return None,
        })
    }
}
