use std::ffi::CStr;
use std::os::raw::c_char;

use j2534_0404_sys::bindings::J2534Api0404;

fn usage(bin: &str) {
    eprintln!("Usage: {bin} <path-to-j2534-dll>");
    eprintln!("Example: {bin} C:\\Vendor\\J2534.dll");
}

fn c_buf_to_string(buf: &[c_char]) -> String {
    let ptr = buf.as_ptr();
    // J2534 version APIs are expected to write NUL-terminated ASCII strings.
    let s = unsafe { CStr::from_ptr(ptr) };
    s.to_string_lossy().into_owned()
}

fn main() {
    let mut args = std::env::args();
    let bin = args.next().unwrap_or_else(|| "read_version".to_string());
    let dll_path = match args.next() {
        Some(path) => path,
        None => {
            usage(&bin);
            std::process::exit(2);
        }
    };

    let api = match unsafe { J2534Api0404::new(&dll_path) } {
        Ok(api) => api,
        Err(err) => {
            eprintln!("failed to load DLL {dll_path}: {err}");
            std::process::exit(1);
        }
    };

    let mut device_id = 0_u32;
    let open_status =
        unsafe { api.PassThruOpen(core::ptr::null_mut(), &mut device_id as *mut u32) };
    if open_status != 0 {
        eprintln!("PassThruOpen failed with status: {open_status:#010x}");
        std::process::exit(1);
    }

    let mut firmware = [0 as c_char; 80];
    let mut dll = [0 as c_char; 80];
    let mut api_version = [0 as c_char; 80];
    let version_status = unsafe {
        api.PassThruReadVersion(
            device_id,
            firmware.as_mut_ptr(),
            dll.as_mut_ptr(),
            api_version.as_mut_ptr(),
        )
    };

    if version_status != 0 {
        eprintln!("PassThruReadVersion failed with status: {version_status:#010x}");
        let _ = unsafe { api.PassThruClose(device_id) };
        std::process::exit(1);
    }

    println!("device_id: {device_id}");
    println!("firmware: {}", c_buf_to_string(&firmware));
    println!("dll: {}", c_buf_to_string(&dll));
    println!("api: {}", c_buf_to_string(&api_version));

    let close_status = unsafe { api.PassThruClose(device_id) };
    if close_status != 0 {
        eprintln!("PassThruClose failed with status: {close_status:#010x}");
        std::process::exit(1);
    }
}
