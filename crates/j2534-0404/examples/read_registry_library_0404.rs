use j2534_0404::J2534Api0404;
use j2534_0404_registry::{RegistryViewMode, enumerate_j2534_devices, find_j2534_device};

fn usage(bin: &str) {
    eprintln!("Usage: {bin} [<device-name>]");
    eprintln!("Example name: ACME Interfaces-Flasher");
}

fn main() {
    let mut args = std::env::args();
    let bin = args
        .next()
        .unwrap_or_else(|| "read_registry_library".to_string());
    let selected_name = args.next();

    if args.next().is_some() {
        usage(&bin);
        std::process::exit(2);
    }

    let devices = match enumerate_j2534_devices(RegistryViewMode::Native) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("failed to enumerate J2534 04.04 devices: {err}");
            std::process::exit(1);
        }
    };

    if devices.is_empty() {
        eprintln!("no J2534 04.04 devices found in registry");
        std::process::exit(1);
    }

    println!("installed J2534 04.04 devices:");
    for d in &devices {
        println!("- {}", d.device_name);
    }

    let chosen_name = match selected_name {
        Some(name) => name,
        None => {
            let first = devices[0].device_name.clone();
            eprintln!("no device name provided; using first entry: {first}");
            first
        }
    };

    let device = match find_j2534_device(&chosen_name) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("failed to find device '{chosen_name}': {err}");
            std::process::exit(1);
        }
    };

    let library_name = device
        .library_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(unknown)".to_string());

    println!();
    println!("selected device: {chosen_name}");
    println!("FunctionLibrary path: {}", device.library_path.display());
    println!("library name: {library_name}");

    match J2534Api0404::from_path(&device.library_path) {
        Ok(_api) => println!("library loaded successfully"),
        Err(err) => eprintln!("failed to load library: {err}"),
    }
}
