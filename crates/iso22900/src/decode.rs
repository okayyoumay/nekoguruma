use iso22900_sys::bindings::{CHAR8, PDU_VERSION_DATA};

use crate::VersionInfo;

fn decode_fixed_string(bytes: &[CHAR8]) -> String {
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    // CHAR8 is i8; reinterpret as u8 in-place to avoid an intermediate Vec allocation.
    let raw = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), len) };
    String::from_utf8_lossy(raw).into_owned()
}

pub(crate) fn decode_version_info(data: &PDU_VERSION_DATA) -> VersionInfo {
    VersionInfo {
        mvci_part_1_standard_version: data.MVCI_Part1StandardVersion,
        mvci_part_2_standard_version: data.MVCI_Part2StandardVersion,
        hardware_serial_number: data.HwSerialNumber,
        hardware_name: decode_fixed_string(&data.HwName),
        hardware_version: data.HwVersion,
        hardware_date: data.HwDate,
        hardware_interface: data.HwInterface,
        firmware_name: decode_fixed_string(&data.FwName),
        firmware_version: data.FwVersion,
        firmware_date: data.FwDate,
        vendor_name: decode_fixed_string(&data.VendorName),
        api_software_name: decode_fixed_string(&data.PDUApiSwName),
        api_software_version: data.PDUApiSwVersion,
        api_software_date: data.PDUApiSwDate,
    }
}
