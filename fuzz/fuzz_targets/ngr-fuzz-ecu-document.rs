//! Arbitrary bytes as an IR declaration part (design 12.1): `load_ecu_document` must refuse
//! malformed input with an error, and a document it accepts must be readable in full (the
//! object API reads every table and vector below the root) and decodable without a panic.
#![no_main]

use diag_ir::declaration::load_ecu_document;
use diag_ir::decode::decode_positive_response;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(document) = load_ecu_document(data) else {
        return;
    };
    let _ = document.unpack();
    let Some(variant) = document.variant() else {
        return;
    };
    for service in variant.services().iter().flatten() {
        if let Some(positive) = service.positive() {
            // The input itself serves as the response, so the fields see arbitrary bytes too.
            let _ = decode_positive_response(&positive, variant.layouts(), data);
        }
    }
});
