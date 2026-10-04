use j2534_0404::J2534Api0404;

fn usage(bin: &str) {
    eprintln!("Usage: {bin} <path-to-j2534-dll>");
    eprintln!("Example: {bin} C:\\Vendor\\J2534.dll");
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

    let version = match api.read_version(device_id) {
        Ok(version) => version,
        Err(err) => {
            eprintln!("PassThruReadVersion failed: {err}");
            let _ = api.close(device_id);
            std::process::exit(1);
        }
    };

    println!("device_id: {}", device_id.0);
    println!("firmware: {}", version.firmware);
    println!("dll: {}", version.dll);
    println!("api: {}", version.api);

    if let Err(err) = api.close(device_id) {
        eprintln!("PassThruClose failed: {err}");
        std::process::exit(1);
    }
}
