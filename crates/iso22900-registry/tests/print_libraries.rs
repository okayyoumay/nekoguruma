//! Example usage for iso22900-registry
//
//! Run with: cargo test -- --nocapture

use iso22900_registry::{RegistryError, RegistryViewMode, enumerate_pdu_libraries};

#[test]
fn print_all_pdu_libraries() {
    let libs = match enumerate_pdu_libraries(RegistryViewMode::Native) {
        Ok(libs) => libs,
        Err(RegistryError::NotFound(_)) => {
            println!("No PDU libraries registered on this system — skipping.");
            return;
        }
        Err(e) => panic!("Failed to enumerate PDU libraries: {e}"),
    };
    for lib in libs {
        println!("Short name: {}", lib.short_name);
        if let Some(desc) = &lib.description {
            println!("  Description: {desc}");
        }
        if let Some(supplier) = &lib.supplier_name {
            println!("  Supplier: {supplier}");
        }
        println!("  Library path: {}", lib.library_path.display());
        if let Some(mdf) = &lib.module_description_path {
            println!("  MDF path: {}", mdf.display());
        }
        if let Some(cdf) = &lib.cable_description_path {
            println!("  CDF path: {}", cdf.display());
        }
        println!();
    }
}
