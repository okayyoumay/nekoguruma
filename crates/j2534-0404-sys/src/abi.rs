//! ABI facade over the generated J2534 v04.04 bindings.
//!
//! The generated bindings (and every caller above them) use `u32` for J2534's
//! `unsigned long`, which is correct on Windows (LLP64) and on 32-bit Linux. On LP64
//! Linux (x86_64, aarch64) a vendor library may instead use a native 64-bit
//! `unsigned long` (`docs/system-architecture.md` 7.1.1 / 7.1.2). [`J2534Api0404`] keeps the
//! `u32`-based call signatures of the generated `bindings::J2534Api0404` and, when the
//! library uses 64-bit `unsigned long`, converts every argument and structure at this
//! one boundary, so that the wrapper and service layers stay unchanged.
//!
//! The width is chosen per process with the environment variable
//! [`LONG_SIZE_ENV`] (`4` or `8`), set by the agent when it launches the worker from
//! the library's registration definition or the ABI table. Unset means `4`, the
//! layout of the generated bindings. `8` is only accepted on LP64 targets.
//!
//! In 64-bit mode, `PassThruIoctl` converts the structures of every IOCTL ID defined by
//! J2534-1 and J2534-2. Other (vendor-specific) IOCTL IDs are passed through only when
//! both data pointers are null; otherwise `ERR_NOT_SUPPORTED` is returned, because their
//! layout is unknown here.

use std::ffi::{OsStr, c_char, c_long, c_void};

use crate::bindings::{self, PASSTHRU_MSG};

/// Environment variable selecting the width of J2534 `unsigned long` in bytes.
pub const LONG_SIZE_ENV: &str = "NGR_J2534_LONG_SIZE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongSize {
    Four,
    #[cfg(lp64)]
    Eight,
}

impl LongSize {
    /// Reads [`LONG_SIZE_ENV`]. An invalid value is reported on stderr and treated as
    /// `4`; the load-time self-check (`PassThruReadVersion`, design doc 7.3) then
    /// catches a wrong interpretation before any vehicle communication.
    pub fn from_env() -> Self {
        match std::env::var(LONG_SIZE_ENV).as_deref() {
            Err(_) | Ok("4") => LongSize::Four,
            #[cfg(lp64)]
            Ok("8") => LongSize::Eight,
            Ok(other) => {
                eprintln!("{LONG_SIZE_ENV}={other:?} is not supported on this target; using 4");
                LongSize::Four
            }
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            LongSize::Four => 4,
            #[cfg(lp64)]
            LongSize::Eight => 8,
        }
    }
}

/// Loaded J2534 v04.04 library with the `u32`-based signatures of the generated bindings.
pub struct J2534Api0404 {
    inner: Inner,
}

enum Inner {
    Native(bindings::J2534Api0404),
    #[cfg(lp64)]
    Long64(long64::Api),
}

macro_rules! dispatch {
    ($self:ident . $f:ident ( $($arg:expr),* )) => {
        match &$self.inner {
            Inner::Native(api) => api.$f($($arg),*),
            #[cfg(lp64)]
            Inner::Long64(api) => api.$f($($arg),*),
        }
    };
}

#[allow(non_snake_case, clippy::missing_safety_doc)]
impl J2534Api0404 {
    /// Loads the library with the `unsigned long` width from [`LongSize::from_env`].
    pub unsafe fn new<P: AsRef<OsStr>>(path: P) -> Result<Self, libloading::Error> {
        unsafe { Self::with_long_size(path, LongSize::from_env()) }
    }

    pub unsafe fn with_long_size<P: AsRef<OsStr>>(
        path: P,
        long_size: LongSize,
    ) -> Result<Self, libloading::Error> {
        let inner = match long_size {
            LongSize::Four => Inner::Native(unsafe { bindings::J2534Api0404::new(path)? }),
            #[cfg(lp64)]
            LongSize::Eight => Inner::Long64(unsafe { long64::Api::new(path)? }),
        };
        Ok(Self { inner })
    }

    pub fn long_size(&self) -> LongSize {
        match self.inner {
            Inner::Native(_) => LongSize::Four,
            #[cfg(lp64)]
            Inner::Long64(_) => LongSize::Eight,
        }
    }

    pub unsafe fn PassThruOpen(&self, pName: *mut c_void, pDeviceID: *mut u32) -> c_long {
        unsafe { dispatch!(self.PassThruOpen(pName, pDeviceID)) }
    }
    pub unsafe fn PassThruClose(&self, DeviceID: u32) -> c_long {
        unsafe { dispatch!(self.PassThruClose(DeviceID)) }
    }
    pub unsafe fn PassThruConnect(
        &self,
        DeviceID: u32,
        ProtocolID: u32,
        Flags: u32,
        BaudRate: u32,
        pChannelID: *mut u32,
    ) -> c_long {
        unsafe {
            dispatch!(self.PassThruConnect(DeviceID, ProtocolID, Flags, BaudRate, pChannelID))
        }
    }
    pub unsafe fn PassThruDisconnect(&self, ChannelID: u32) -> c_long {
        unsafe { dispatch!(self.PassThruDisconnect(ChannelID)) }
    }
    pub unsafe fn PassThruReadMsgs(
        &self,
        ChannelID: u32,
        pMsg: *mut PASSTHRU_MSG,
        pNumMsgs: *mut u32,
        Timeout: u32,
    ) -> c_long {
        unsafe { dispatch!(self.PassThruReadMsgs(ChannelID, pMsg, pNumMsgs, Timeout)) }
    }
    pub unsafe fn PassThruWriteMsgs(
        &self,
        ChannelID: u32,
        pMsg: *mut PASSTHRU_MSG,
        pNumMsgs: *mut u32,
        Timeout: u32,
    ) -> c_long {
        unsafe { dispatch!(self.PassThruWriteMsgs(ChannelID, pMsg, pNumMsgs, Timeout)) }
    }
    pub unsafe fn PassThruStartPeriodicMsg(
        &self,
        ChannelID: u32,
        pMsg: *mut PASSTHRU_MSG,
        pMsgID: *mut u32,
        TimeInterval: u32,
    ) -> c_long {
        unsafe { dispatch!(self.PassThruStartPeriodicMsg(ChannelID, pMsg, pMsgID, TimeInterval)) }
    }
    pub unsafe fn PassThruStopPeriodicMsg(&self, ChannelID: u32, MsgID: u32) -> c_long {
        unsafe { dispatch!(self.PassThruStopPeriodicMsg(ChannelID, MsgID)) }
    }
    pub unsafe fn PassThruStartMsgFilter(
        &self,
        ChannelID: u32,
        FilterType: u32,
        pMaskMsg: *mut PASSTHRU_MSG,
        pPatternMsg: *mut PASSTHRU_MSG,
        pFlowControlMsg: *mut PASSTHRU_MSG,
        pFilterID: *mut u32,
    ) -> c_long {
        unsafe {
            dispatch!(self.PassThruStartMsgFilter(
                ChannelID,
                FilterType,
                pMaskMsg,
                pPatternMsg,
                pFlowControlMsg,
                pFilterID
            ))
        }
    }
    pub unsafe fn PassThruStopMsgFilter(&self, ChannelID: u32, FilterID: u32) -> c_long {
        unsafe { dispatch!(self.PassThruStopMsgFilter(ChannelID, FilterID)) }
    }
    pub unsafe fn PassThruSetProgrammingVoltage(
        &self,
        DeviceID: u32,
        PinNumber: u32,
        Voltage: u32,
    ) -> c_long {
        unsafe { dispatch!(self.PassThruSetProgrammingVoltage(DeviceID, PinNumber, Voltage)) }
    }
    pub unsafe fn PassThruReadVersion(
        &self,
        DeviceID: u32,
        pFirmwareVersion: *mut c_char,
        pDllVersion: *mut c_char,
        pApiVersion: *mut c_char,
    ) -> c_long {
        unsafe {
            dispatch!(self.PassThruReadVersion(
                DeviceID,
                pFirmwareVersion,
                pDllVersion,
                pApiVersion
            ))
        }
    }
    pub unsafe fn PassThruGetLastError(&self, pErrorDescription: *mut c_char) -> c_long {
        unsafe { dispatch!(self.PassThruGetLastError(pErrorDescription)) }
    }
    pub unsafe fn PassThruIoctl(
        &self,
        ChannelID: u32,
        IoctlID: u32,
        pInput: *mut c_void,
        pOutput: *mut c_void,
    ) -> c_long {
        unsafe { dispatch!(self.PassThruIoctl(ChannelID, IoctlID, pInput, pOutput)) }
    }
}

#[cfg(lp64)]
mod long64 {
    //! J2534 with 64-bit `unsigned long`: shadow structures and conversions.

    use super::*;
    use crate::bindings::{SBYTE_ARRAY, SCONFIG, SCONFIG_LIST, SPARAM, SPARAM_LIST};

    type Ul = std::ffi::c_ulong;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct Msg {
        pub protocol_id: Ul,
        pub rx_status: Ul,
        pub tx_flags: Ul,
        pub timestamp: Ul,
        pub data_size: Ul,
        pub extra_data_index: Ul,
        pub data: [u8; 4128],
    }

    #[repr(C)]
    pub(super) struct Config {
        pub parameter: Ul,
        pub value: Ul,
    }

    #[repr(C)]
    pub(super) struct ConfigList {
        pub num_of_params: Ul,
        pub config_ptr: *mut Config,
    }

    #[repr(C)]
    pub(super) struct ByteArray {
        pub num_of_bytes: Ul,
        pub byte_ptr: *mut u8,
    }

    #[repr(C)]
    pub(super) struct Param {
        pub parameter: Ul,
        pub value: Ul,
        pub supported: Ul,
    }

    #[repr(C)]
    pub(super) struct ParamList {
        pub num_of_params: Ul,
        pub param_ptr: *mut Param,
    }

    #[repr(C)]
    pub(super) struct RepeatSetup {
        pub time_interval: Ul,
        pub condition: Ul,
        pub repeat_msg_data: [Msg; 3],
    }

    #[repr(C)]
    pub(super) struct NdisInfo {
        pub adapter_unique_id: [c_char; 128],
        pub adapter_name: [c_char; 64],
        pub status: Ul,
        pub mac_address: [u8; 6],
        pub ipv6_address: [u8; 16],
        pub ipv4_address: [u8; 4],
        pub ethernet_pin_config: Ul,
    }

    // Values coming back from the library are narrowed to the 32-bit API above it.
    // J2534 values are defined within 32 bits, so the high half is expected to be zero.
    fn n(v: Ul) -> u32 {
        v as u32
    }

    pub(super) fn msg_to64(m: &PASSTHRU_MSG) -> Msg {
        Msg {
            protocol_id: m.ProtocolID.into(),
            rx_status: m.RxStatus.into(),
            tx_flags: m.TxFlags.into(),
            timestamp: m.Timestamp.into(),
            data_size: m.DataSize.into(),
            extra_data_index: m.ExtraDataIndex.into(),
            data: m.Data,
        }
    }

    pub(super) fn msg_from64(m: &Msg) -> PASSTHRU_MSG {
        PASSTHRU_MSG {
            ProtocolID: n(m.protocol_id),
            RxStatus: n(m.rx_status),
            TxFlags: n(m.tx_flags),
            Timestamp: n(m.timestamp),
            DataSize: n(m.data_size),
            ExtraDataIndex: n(m.extra_data_index),
            Data: m.data,
        }
    }

    fn zero_msg() -> Msg {
        Msg {
            protocol_id: 0,
            rx_status: 0,
            tx_flags: 0,
            timestamp: 0,
            data_size: 0,
            extra_data_index: 0,
            data: [0; 4128],
        }
    }

    /// Converts an optional input message; the returned box must outlive the call.
    unsafe fn opt_msg(p: *const PASSTHRU_MSG) -> Option<Box<Msg>> {
        unsafe { p.as_ref() }.map(|m| Box::new(msg_to64(m)))
    }

    fn ptr_or_null<T>(b: &mut Option<Box<T>>) -> *mut T {
        b.as_deref_mut()
            .map_or(std::ptr::null_mut(), |r| r as *mut T)
    }

    type FOpen = unsafe extern "C" fn(*mut c_void, *mut Ul) -> c_long;
    type FHandle = unsafe extern "C" fn(Ul) -> c_long;
    type FConnect = unsafe extern "C" fn(Ul, Ul, Ul, Ul, *mut Ul) -> c_long;
    type FMsgs = unsafe extern "C" fn(Ul, *mut Msg, *mut Ul, Ul) -> c_long;
    type FPeriodic = unsafe extern "C" fn(Ul, *mut Msg, *mut Ul, Ul) -> c_long;
    type FStop = unsafe extern "C" fn(Ul, Ul) -> c_long;
    type FFilter = unsafe extern "C" fn(Ul, Ul, *mut Msg, *mut Msg, *mut Msg, *mut Ul) -> c_long;
    type FVoltage = unsafe extern "C" fn(Ul, Ul, Ul) -> c_long;
    type FVersion = unsafe extern "C" fn(Ul, *mut c_char, *mut c_char, *mut c_char) -> c_long;
    type FLastError = unsafe extern "C" fn(*mut c_char) -> c_long;
    type FIoctl = unsafe extern "C" fn(Ul, Ul, *mut c_void, *mut c_void) -> c_long;

    pub(super) struct Fns {
        pub open: FOpen,
        pub close: FHandle,
        pub connect: FConnect,
        pub disconnect: FHandle,
        pub read_msgs: FMsgs,
        pub write_msgs: FMsgs,
        pub start_periodic: FPeriodic,
        pub stop_periodic: FStop,
        pub start_filter: FFilter,
        pub stop_filter: FStop,
        pub set_voltage: FVoltage,
        pub read_version: FVersion,
        pub last_error: FLastError,
        pub ioctl: FIoctl,
    }

    pub(super) struct Api {
        _library: Option<libloading::Library>,
        f: Fns,
    }

    const ERR_NOT_SUPPORTED: c_long = bindings::ERR_NOT_SUPPORTED as c_long;
    const ERR_NULL_PARAMETER: c_long = bindings::ERR_NULL_PARAMETER as c_long;

    #[allow(non_snake_case)]
    impl Api {
        pub unsafe fn new<P: AsRef<OsStr>>(path: P) -> Result<Self, libloading::Error> {
            unsafe {
                let lib = libloading::Library::new(path.as_ref())?;
                macro_rules! sym {
                    ($name:literal) => {
                        *lib.get($name)?
                    };
                }
                let f = Fns {
                    open: sym!(b"PassThruOpen\0"),
                    close: sym!(b"PassThruClose\0"),
                    connect: sym!(b"PassThruConnect\0"),
                    disconnect: sym!(b"PassThruDisconnect\0"),
                    read_msgs: sym!(b"PassThruReadMsgs\0"),
                    write_msgs: sym!(b"PassThruWriteMsgs\0"),
                    start_periodic: sym!(b"PassThruStartPeriodicMsg\0"),
                    stop_periodic: sym!(b"PassThruStopPeriodicMsg\0"),
                    start_filter: sym!(b"PassThruStartMsgFilter\0"),
                    stop_filter: sym!(b"PassThruStopMsgFilter\0"),
                    set_voltage: sym!(b"PassThruSetProgrammingVoltage\0"),
                    read_version: sym!(b"PassThruReadVersion\0"),
                    last_error: sym!(b"PassThruGetLastError\0"),
                    ioctl: sym!(b"PassThruIoctl\0"),
                };
                Ok(Self {
                    _library: Some(lib),
                    f,
                })
            }
        }

        #[cfg(test)]
        pub fn from_fns(f: Fns) -> Self {
            Self { _library: None, f }
        }

        pub unsafe fn PassThruOpen(&self, pName: *mut c_void, pDeviceID: *mut u32) -> c_long {
            out_u32(pDeviceID, |p| unsafe { (self.f.open)(pName, p) })
        }
        pub unsafe fn PassThruClose(&self, id: u32) -> c_long {
            unsafe { (self.f.close)(id.into()) }
        }
        pub unsafe fn PassThruConnect(
            &self,
            DeviceID: u32,
            ProtocolID: u32,
            Flags: u32,
            BaudRate: u32,
            pChannelID: *mut u32,
        ) -> c_long {
            out_u32(pChannelID, |p| unsafe {
                (self.f.connect)(
                    DeviceID.into(),
                    ProtocolID.into(),
                    Flags.into(),
                    BaudRate.into(),
                    p,
                )
            })
        }
        pub unsafe fn PassThruDisconnect(&self, id: u32) -> c_long {
            unsafe { (self.f.disconnect)(id.into()) }
        }

        pub unsafe fn PassThruReadMsgs(
            &self,
            ChannelID: u32,
            pMsg: *mut PASSTHRU_MSG,
            pNumMsgs: *mut u32,
            Timeout: u32,
        ) -> c_long {
            if pMsg.is_null() || pNumMsgs.is_null() {
                return unsafe {
                    (self.f.read_msgs)(
                        ChannelID.into(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        Timeout.into(),
                    )
                };
            }
            let capacity = unsafe { *pNumMsgs } as usize;
            let mut buf: Vec<Msg> = vec![zero_msg(); capacity];
            let mut num: Ul = capacity as Ul;
            let status = unsafe {
                (self.f.read_msgs)(ChannelID.into(), buf.as_mut_ptr(), &mut num, Timeout.into())
            };
            let read = (num as usize).min(capacity);
            for (i, m) in buf.iter().take(read).enumerate() {
                unsafe { pMsg.add(i).write(msg_from64(m)) };
            }
            unsafe { *pNumMsgs = read as u32 };
            status
        }

        pub unsafe fn PassThruWriteMsgs(
            &self,
            ChannelID: u32,
            pMsg: *mut PASSTHRU_MSG,
            pNumMsgs: *mut u32,
            Timeout: u32,
        ) -> c_long {
            if pMsg.is_null() || pNumMsgs.is_null() {
                return unsafe {
                    (self.f.write_msgs)(
                        ChannelID.into(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        Timeout.into(),
                    )
                };
            }
            let count = unsafe { *pNumMsgs } as usize;
            let mut buf: Vec<Msg> = (0..count)
                .map(|i| msg_to64(unsafe { &*pMsg.add(i) }))
                .collect();
            let mut num: Ul = count as Ul;
            let status = unsafe {
                (self.f.write_msgs)(ChannelID.into(), buf.as_mut_ptr(), &mut num, Timeout.into())
            };
            unsafe { *pNumMsgs = n(num) };
            status
        }

        pub unsafe fn PassThruStartPeriodicMsg(
            &self,
            ChannelID: u32,
            pMsg: *mut PASSTHRU_MSG,
            pMsgID: *mut u32,
            TimeInterval: u32,
        ) -> c_long {
            let mut msg = unsafe { opt_msg(pMsg) };
            out_u32(pMsgID, |p| unsafe {
                (self.f.start_periodic)(
                    ChannelID.into(),
                    ptr_or_null(&mut msg),
                    p,
                    TimeInterval.into(),
                )
            })
        }
        pub unsafe fn PassThruStopPeriodicMsg(&self, ChannelID: u32, MsgID: u32) -> c_long {
            unsafe { (self.f.stop_periodic)(ChannelID.into(), MsgID.into()) }
        }

        pub unsafe fn PassThruStartMsgFilter(
            &self,
            ChannelID: u32,
            FilterType: u32,
            pMaskMsg: *mut PASSTHRU_MSG,
            pPatternMsg: *mut PASSTHRU_MSG,
            pFlowControlMsg: *mut PASSTHRU_MSG,
            pFilterID: *mut u32,
        ) -> c_long {
            let (mut mask, mut pattern, mut flow) = unsafe {
                (
                    opt_msg(pMaskMsg),
                    opt_msg(pPatternMsg),
                    opt_msg(pFlowControlMsg),
                )
            };
            out_u32(pFilterID, |p| unsafe {
                (self.f.start_filter)(
                    ChannelID.into(),
                    FilterType.into(),
                    ptr_or_null(&mut mask),
                    ptr_or_null(&mut pattern),
                    ptr_or_null(&mut flow),
                    p,
                )
            })
        }
        pub unsafe fn PassThruStopMsgFilter(&self, ChannelID: u32, FilterID: u32) -> c_long {
            unsafe { (self.f.stop_filter)(ChannelID.into(), FilterID.into()) }
        }
        pub unsafe fn PassThruSetProgrammingVoltage(
            &self,
            DeviceID: u32,
            PinNumber: u32,
            Voltage: u32,
        ) -> c_long {
            unsafe { (self.f.set_voltage)(DeviceID.into(), PinNumber.into(), Voltage.into()) }
        }
        pub unsafe fn PassThruReadVersion(
            &self,
            DeviceID: u32,
            a: *mut c_char,
            b: *mut c_char,
            c: *mut c_char,
        ) -> c_long {
            unsafe { (self.f.read_version)(DeviceID.into(), a, b, c) }
        }
        pub unsafe fn PassThruGetLastError(&self, p: *mut c_char) -> c_long {
            unsafe { (self.f.last_error)(p) }
        }

        pub unsafe fn PassThruIoctl(
            &self,
            Handle: u32,
            IoctlID: u32,
            pInput: *mut c_void,
            pOutput: *mut c_void,
        ) -> c_long {
            use bindings::*;
            let h: Ul = Handle.into();
            let id: Ul = IoctlID.into();
            let call = |i: *mut c_void, o: *mut c_void| unsafe { (self.f.ioctl)(h, id, i, o) };
            unsafe {
                match IoctlID {
                    IOCTL_GET_CONFIG
                    | IOCTL_SET_CONFIG
                    | IOCTL_GET_DEVICE_CONFIG
                    | IOCTL_SET_DEVICE_CONFIG => {
                        with_config_list(pInput.cast(), |i| call(i, pOutput))
                    }
                    IOCTL_READ_VBATT | IOCTL_READ_PROG_VOLTAGE => {
                        out_u32(pOutput.cast(), |o| call(pInput, o.cast()))
                    }
                    IOCTL_QUERY_REPEAT_MESSAGE | IOCTL_READ_J1962PIN_VOLTAGE => {
                        in_u32(pInput.cast(), |i| {
                            out_u32(pOutput.cast(), |o| call(i.cast(), o.cast()))
                        })
                    }
                    IOCTL_STOP_REPEAT_MESSAGE => in_u32(pInput.cast(), |i| call(i.cast(), pOutput)),
                    IOCTL_FIVE_BAUD_INIT => with_byte_array(pInput.cast(), |i| {
                        with_byte_array(pOutput.cast(), |o| call(i, o))
                    }),
                    IOCTL_ADD_TO_FUNCT_MSG_LOOKUP_TABLE
                    | IOCTL_DELETE_FROM_FUNCT_MSG_LOOKUP_TABLE
                    | IOCTL_SET_POLL_RESPONSE
                    | IOCTL_BECOME_MASTER
                    | IOCTL_PROTECT_J1939_ADDR
                    | IOCTL_REQUEST_CONNECTION
                    | IOCTL_TEARDOWN_CONNECTION => {
                        with_byte_array(pInput.cast(), |i| call(i, pOutput))
                    }
                    IOCTL_FAST_INIT => {
                        let mut input = opt_msg(pInput.cast());
                        let out_ptr: *mut PASSTHRU_MSG = pOutput.cast();
                        let mut output = out_ptr.as_ref().map(|m| Box::new(msg_to64(m)));
                        let status = call(
                            ptr_or_null(&mut input).cast(),
                            ptr_or_null(&mut output).cast(),
                        );
                        if let (Some(dst), Some(src)) = (out_ptr.as_mut(), output.as_deref()) {
                            *dst = msg_from64(src);
                        }
                        status
                    }
                    IOCTL_START_REPEAT_MESSAGE => {
                        let Some(setup) = (pInput as *const REPEAT_MSG_SETUP).as_ref() else {
                            return out_u32(pOutput.cast(), |o| {
                                call(std::ptr::null_mut(), o.cast())
                            });
                        };
                        let mut setup64 = Box::new(RepeatSetup {
                            time_interval: setup.TimeInterval.into(),
                            condition: setup.Condition.into(),
                            repeat_msg_data: setup.RepeatMsgData.each_ref().map(msg_to64),
                        });
                        out_u32(pOutput.cast(), |o| {
                            call((&mut *setup64 as *mut RepeatSetup).cast(), o.cast())
                        })
                    }
                    IOCTL_GET_DEVICE_INFO => with_param_list(pOutput.cast(), |o| call(pInput, o)),
                    IOCTL_GET_PROTOCOL_INFO => in_u32(pInput.cast(), |i| {
                        with_param_list(pOutput.cast(), |o| call(i.cast(), o))
                    }),
                    IOCTL_GET_NDIS_ADAPTER_INFO => {
                        let out: *mut NDIS_ADAPTER_INFORMATION = pOutput.cast();
                        let Some(dst) = out.as_mut() else {
                            return call(pInput, std::ptr::null_mut());
                        };
                        let mut info = Box::new(NdisInfo {
                            adapter_unique_id: dst.AdapterUniqueID,
                            adapter_name: dst.AdapterName,
                            status: dst.Status.into(),
                            mac_address: dst.MAC_Address,
                            ipv6_address: dst.IPV6_Address,
                            ipv4_address: dst.IPV4_Address,
                            ethernet_pin_config: dst.EthernetPinConfig.into(),
                        });
                        let status = call(pInput, (&mut *info as *mut NdisInfo).cast());
                        dst.AdapterUniqueID = info.adapter_unique_id;
                        dst.AdapterName = info.adapter_name;
                        dst.Status = n(info.status);
                        dst.MAC_Address = info.mac_address;
                        dst.IPV6_Address = info.ipv6_address;
                        dst.IPV4_Address = info.ipv4_address;
                        dst.EthernetPinConfig = n(info.ethernet_pin_config);
                        status
                    }
                    // No data: CLEAR_*, SW_CAN_HS / SW_CAN_NS, and vendor IOCTLs without data.
                    _ if pInput.is_null() && pOutput.is_null() => call(pInput, pOutput),
                    IOCTL_CLEAR_TX_BUFFER
                    | IOCTL_CLEAR_RX_BUFFER
                    | IOCTL_CLEAR_PERIODIC_MSGS
                    | IOCTL_CLEAR_MSG_FILTERS
                    | IOCTL_CLEAR_FUNCT_MSG_LOOKUP_TABLE
                    | IOCTL_SW_CAN_HS
                    | IOCTL_SW_CAN_NS => call(pInput, pOutput),
                    _ => super::long64::ERR_NOT_SUPPORTED,
                }
            }
        }
    }

    /// Calls `f` with a 64-bit out-slot and narrows the result into `p` (null passes null).
    fn out_u32(p: *mut u32, f: impl FnOnce(*mut Ul) -> c_long) -> c_long {
        if p.is_null() {
            return f(std::ptr::null_mut());
        }
        let mut v: Ul = unsafe { *p }.into();
        let status = f(&mut v);
        unsafe { *p = n(v) };
        status
    }

    /// Like [`out_u32`] for an in/out value read by the library.
    fn in_u32(p: *mut u32, f: impl FnOnce(*mut Ul) -> c_long) -> c_long {
        out_u32(p, f)
    }

    unsafe fn with_config_list(
        p: *mut SCONFIG_LIST,
        f: impl FnOnce(*mut c_void) -> c_long,
    ) -> c_long {
        let Some(list) = (unsafe { p.as_mut() }) else {
            return f(std::ptr::null_mut());
        };
        if list.ConfigPtr.is_null() && list.NumOfParams != 0 {
            return ERR_NULL_PARAMETER;
        }
        let items: &mut [SCONFIG] = if list.NumOfParams == 0 {
            &mut []
        } else {
            unsafe { std::slice::from_raw_parts_mut(list.ConfigPtr, list.NumOfParams as usize) }
        };
        let mut configs: Vec<Config> = items
            .iter()
            .map(|c| Config {
                parameter: c.Parameter.into(),
                value: c.Value.into(),
            })
            .collect();
        let mut list64 = ConfigList {
            num_of_params: configs.len() as Ul,
            config_ptr: configs.as_mut_ptr(),
        };
        let status = f((&mut list64 as *mut ConfigList).cast());
        for (dst, src) in items.iter_mut().zip(&configs) {
            dst.Parameter = n(src.parameter);
            dst.Value = n(src.value);
        }
        status
    }

    unsafe fn with_byte_array(
        p: *mut SBYTE_ARRAY,
        f: impl FnOnce(*mut c_void) -> c_long,
    ) -> c_long {
        let Some(array) = (unsafe { p.as_mut() }) else {
            return f(std::ptr::null_mut());
        };
        let mut array64 = ByteArray {
            num_of_bytes: array.NumOfBytes.into(),
            byte_ptr: array.BytePtr,
        };
        let status = f((&mut array64 as *mut ByteArray).cast());
        array.NumOfBytes = n(array64.num_of_bytes);
        status
    }

    unsafe fn with_param_list(
        p: *mut SPARAM_LIST,
        f: impl FnOnce(*mut c_void) -> c_long,
    ) -> c_long {
        let Some(list) = (unsafe { p.as_mut() }) else {
            return f(std::ptr::null_mut());
        };
        if list.ParamPtr.is_null() && list.NumOfParams != 0 {
            return ERR_NULL_PARAMETER;
        }
        let items: &mut [SPARAM] = if list.NumOfParams == 0 {
            &mut []
        } else {
            unsafe { std::slice::from_raw_parts_mut(list.ParamPtr, list.NumOfParams as usize) }
        };
        let mut params: Vec<Param> = items
            .iter()
            .map(|p| Param {
                parameter: p.Parameter.into(),
                value: p.Value.into(),
                supported: p.Supported.into(),
            })
            .collect();
        let mut list64 = ParamList {
            num_of_params: params.len() as Ul,
            param_ptr: params.as_mut_ptr(),
        };
        let status = f((&mut list64 as *mut ParamList).cast());
        for (dst, src) in items.iter_mut().zip(&params) {
            dst.Parameter = n(src.parameter);
            dst.Value = n(src.value);
            dst.Supported = n(src.supported);
        }
        status
    }

    #[cfg(test)]
    mod tests {
        //! The fake library functions below take 64-bit `unsigned long` and write full
        //! 64-bit values, so a missing conversion shows up as corrupted neighbours.

        use super::*;

        const BIG: Ul = 0xDEAD_BEEF_0000_0000;

        unsafe extern "C" fn open(_: *mut c_void, id: *mut Ul) -> c_long {
            unsafe { *id = 7 };
            0
        }
        unsafe extern "C" fn handle(h: Ul) -> c_long {
            if h == 7 { 0 } else { 1 }
        }
        unsafe extern "C" fn connect(d: Ul, p: Ul, f: Ul, b: Ul, ch: *mut Ul) -> c_long {
            assert_eq!((d, p, f, b), (7, 6, 0x100, 500_000));
            unsafe { *ch = 9 };
            0
        }
        unsafe extern "C" fn read_msgs(_: Ul, m: *mut Msg, num: *mut Ul, _: Ul) -> c_long {
            let cap = unsafe { *num };
            assert!(cap >= 2);
            for i in 0..2 {
                let msg = unsafe { &mut *m.add(i) };
                msg.protocol_id = 6;
                msg.rx_status = 2;
                msg.timestamp = 1000 + i as Ul;
                msg.data_size = 3;
                msg.data[..3].copy_from_slice(&[0x7E, 0x80, i as u8]);
            }
            unsafe { *num = 2 };
            0
        }
        unsafe extern "C" fn write_msgs(_: Ul, m: *mut Msg, num: *mut Ul, _: Ul) -> c_long {
            let msg = unsafe { &*m };
            assert_eq!((msg.protocol_id, msg.tx_flags, msg.data_size), (6, 0x40, 2));
            assert_eq!(unsafe { *num }, 1);
            0
        }
        unsafe extern "C" fn periodic(_: Ul, m: *mut Msg, id: *mut Ul, interval: Ul) -> c_long {
            assert_eq!((unsafe { (*m).data_size }, interval), (2, 100));
            unsafe { *id = 3 };
            0
        }
        unsafe extern "C" fn stop(_: Ul, _: Ul) -> c_long {
            0
        }
        unsafe extern "C" fn filter(
            _: Ul,
            t: Ul,
            mask: *mut Msg,
            pat: *mut Msg,
            flow: *mut Msg,
            id: *mut Ul,
        ) -> c_long {
            assert_eq!(t, 3);
            assert!(!mask.is_null() && !pat.is_null() && !flow.is_null());
            unsafe { *id = 4 };
            0
        }
        unsafe extern "C" fn voltage(_: Ul, _: Ul, _: Ul) -> c_long {
            0
        }
        unsafe extern "C" fn version(
            _: Ul,
            _: *mut c_char,
            _: *mut c_char,
            _: *mut c_char,
        ) -> c_long {
            0
        }
        unsafe extern "C" fn last_error(_: *mut c_char) -> c_long {
            0
        }
        unsafe extern "C" fn ioctl(
            _: Ul,
            id: Ul,
            input: *mut c_void,
            output: *mut c_void,
        ) -> c_long {
            use bindings::*;
            match id as u32 {
                IOCTL_GET_CONFIG => {
                    let list = unsafe { &*(input as *const ConfigList) };
                    for i in 0..list.num_of_params as usize {
                        let c = unsafe { &mut *list.config_ptr.add(i) };
                        c.value = c.parameter * 10;
                    }
                    0
                }
                IOCTL_READ_VBATT => {
                    unsafe { *(output as *mut Ul) = 12_000 };
                    0
                }
                IOCTL_FIVE_BAUD_INIT => {
                    let i = unsafe { &*(input as *const ByteArray) };
                    let o = unsafe { &mut *(output as *mut ByteArray) };
                    assert_eq!((i.num_of_bytes, unsafe { *i.byte_ptr }), (1, 0x33));
                    unsafe {
                        *o.byte_ptr = 0x08;
                        *o.byte_ptr.add(1) = 0x8F
                    };
                    o.num_of_bytes = 2;
                    0
                }
                IOCTL_GET_PROTOCOL_INFO => {
                    assert_eq!(unsafe { *(input as *const Ul) }, 6);
                    let list = unsafe { &*(output as *const ParamList) };
                    let p = unsafe { &mut *list.param_ptr };
                    p.value = 42;
                    p.supported = 1;
                    0
                }
                IOCTL_QUERY_REPEAT_MESSAGE => {
                    assert_eq!(unsafe { *(input as *const Ul) }, 5);
                    unsafe { *(output as *mut Ul) = 1 };
                    0
                }
                IOCTL_CLEAR_RX_BUFFER => 0,
                _ => BIG as c_long,
            }
        }

        fn api() -> Api {
            Api::from_fns(Fns {
                open,
                close: handle,
                connect,
                disconnect: handle,
                read_msgs,
                write_msgs,
                start_periodic: periodic,
                stop_periodic: stop,
                start_filter: filter,
                stop_filter: stop,
                set_voltage: voltage,
                read_version: version,
                last_error,
                ioctl,
            })
        }

        fn msg(protocol: u32, tx_flags: u32, data: &[u8]) -> PASSTHRU_MSG {
            let mut m = msg_from64(&zero_msg());
            m.ProtocolID = protocol;
            m.TxFlags = tx_flags;
            m.DataSize = data.len() as u32;
            m.Data[..data.len()].copy_from_slice(data);
            m
        }

        #[test]
        fn handles_and_ids() {
            let api = api();
            let mut device = 0u32;
            let mut channel = 0u32;
            unsafe {
                assert_eq!(api.PassThruOpen(std::ptr::null_mut(), &mut device), 0);
                assert_eq!(
                    api.PassThruConnect(device, 6, 0x100, 500_000, &mut channel),
                    0
                );
                assert_eq!(api.PassThruClose(device), 0);
            }
            assert_eq!((device, channel), (7, 9));
        }

        #[test]
        fn messages() {
            let api = api();
            let mut out = [msg(0, 0, &[]), msg(0, 0, &[]), msg(0, 0, &[])];
            let mut num = 3u32;
            assert_eq!(
                unsafe { api.PassThruReadMsgs(9, out.as_mut_ptr(), &mut num, 0) },
                0
            );
            assert_eq!(num, 2);
            assert_eq!(
                (out[1].ProtocolID, out[1].Timestamp, out[1].DataSize),
                (6, 1001, 3)
            );
            assert_eq!(&out[1].Data[..3], &[0x7E, 0x80, 1]);

            let mut tx = msg(6, 0x40, &[0x3E, 0x00]);
            let mut num = 1u32;
            assert_eq!(unsafe { api.PassThruWriteMsgs(9, &mut tx, &mut num, 0) }, 0);

            let mut id = 0u32;
            assert_eq!(
                unsafe { api.PassThruStartPeriodicMsg(9, &mut tx, &mut id, 100) },
                0
            );
            assert_eq!(id, 3);

            let (mut a, mut b, mut c) = (tx, tx, tx);
            let mut filter = 0u32;
            assert_eq!(
                unsafe { api.PassThruStartMsgFilter(9, 3, &mut a, &mut b, &mut c, &mut filter) },
                0
            );
            assert_eq!(filter, 4);
        }

        #[test]
        fn ioctls() {
            let api = api();
            unsafe {
                let mut configs = [
                    SCONFIG {
                        Parameter: 1,
                        Value: 0,
                    },
                    SCONFIG {
                        Parameter: 0x1F,
                        Value: 0,
                    },
                ];
                let mut list = SCONFIG_LIST {
                    NumOfParams: 2,
                    ConfigPtr: configs.as_mut_ptr(),
                };
                let status = api.PassThruIoctl(
                    9,
                    bindings::IOCTL_GET_CONFIG,
                    (&mut list as *mut SCONFIG_LIST).cast(),
                    std::ptr::null_mut(),
                );
                assert_eq!(status, 0);
                assert_eq!((configs[0].Value, configs[1].Value), (10, 0x136));

                // A u32 slot next to the output: a 64-bit write without conversion would clobber it.
                let mut slots = [0u32, 0xAAAA_AAAA];
                api.PassThruIoctl(
                    7,
                    bindings::IOCTL_READ_VBATT,
                    std::ptr::null_mut(),
                    slots.as_mut_ptr().cast(),
                );
                assert_eq!(slots, [12_000, 0xAAAA_AAAA]);

                let mut addr = 0x33u8;
                let mut keywords = [0u8; 2];
                let mut input = SBYTE_ARRAY {
                    NumOfBytes: 1,
                    BytePtr: &mut addr,
                };
                let mut output = SBYTE_ARRAY {
                    NumOfBytes: 2,
                    BytePtr: keywords.as_mut_ptr(),
                };
                api.PassThruIoctl(
                    9,
                    bindings::IOCTL_FIVE_BAUD_INIT,
                    (&mut input as *mut SBYTE_ARRAY).cast(),
                    (&mut output as *mut SBYTE_ARRAY).cast(),
                );
                assert_eq!((keywords, output.NumOfBytes), ([0x08, 0x8F], 2));

                let mut protocol = 6u32;
                let mut params = [SPARAM {
                    Parameter: 1,
                    Value: 0,
                    Supported: 0,
                }];
                let mut plist = SPARAM_LIST {
                    NumOfParams: 1,
                    ParamPtr: params.as_mut_ptr(),
                };
                api.PassThruIoctl(
                    7,
                    bindings::IOCTL_GET_PROTOCOL_INFO,
                    (&mut protocol as *mut u32).cast(),
                    (&mut plist as *mut SPARAM_LIST).cast(),
                );
                assert_eq!((params[0].Value, params[0].Supported), (42, 1));

                let mut msg_id = [5u32, 0xBBBB_BBBB];
                let mut status = [0u32, 0xCCCC_CCCC];
                api.PassThruIoctl(
                    9,
                    bindings::IOCTL_QUERY_REPEAT_MESSAGE,
                    msg_id.as_mut_ptr().cast(),
                    status.as_mut_ptr().cast(),
                );
                assert_eq!((msg_id, status), ([5, 0xBBBB_BBBB], [1, 0xCCCC_CCCC]));

                assert_eq!(
                    api.PassThruIoctl(
                        9,
                        bindings::IOCTL_CLEAR_RX_BUFFER,
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    0
                );
                let mut vendor = 0u32;
                assert_eq!(
                    api.PassThruIoctl(
                        9,
                        0x0001_0000,
                        (&mut vendor as *mut u32).cast(),
                        std::ptr::null_mut()
                    ),
                    ERR_NOT_SUPPORTED
                );
            }
        }
    }
}
