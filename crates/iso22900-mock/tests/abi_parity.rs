//! ABI parity between `iso22900-mock`'s exported D-PDU API functions and the
//! function-pointer types the `iso22900-sys` bindings load them as.
//!
//! `iso22900` loads the mock through `DPduApiSys`, whose fields are the
//! bindgen-generated pointer types of the real API, including the calling
//! convention of the target. Each check below coerces one mock export to its
//! field's type, so this file stops compiling if an export's parameter list,
//! return type or calling convention drifts from the bindings, for example on
//! a target where the mock's `exported_fn!` macro picks another ABI. The
//! callback type is checked through `PDURegisterEventCallback`, whose
//! parameter is the bindings' `CALLBACKFNC`.

use std::collections::BTreeSet;
use std::marker::PhantomData;

use iso22900_sys::bindings::{DPduApiSys, T_PDU_IT, UNUM32};

/// The type of the `DPduApiSys` field that `field` reads.
fn field_type<F>(_field: fn(&DPduApiSys) -> F) -> PhantomData<F> {
    PhantomData
}

/// Compiles only if `export` coerces to `F`. `F` is fixed by the first
/// argument, so the export (a function item) is coerced to the field's
/// pointer type instead of `F` being inferred from the export.
fn coerces_to<F>(_field: PhantomData<F>, _export: F) {}

/// Checks each export and returns the names it checked.
macro_rules! assert_parity {
    ($($name:ident),* $(,)?) => {{
        $(coerces_to(field_type(|api| api.$name), iso22900_mock::$name);)*
        [$(stringify!($name)),*]
    }};
}

/// The C header the bindings are generated from. Bindgen generates a
/// `DPduApiSys` entry point for every `PDU*` function it declares
/// (`crates/iso22900-sys/build.rs`), so it lists every entry point.
const API_HEADER: &str = include_str!("../../iso22900-sys/src/bindings/d_pdu_api_func.h");

/// Names of the functions the header declares: each `PDU...` identifier
/// followed by `(`, with or without whitespace in between.
fn header_functions() -> BTreeSet<&'static str> {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut names = BTreeSet::new();
    for (at, _) in API_HEADER.match_indices("PDU") {
        if API_HEADER[..at].chars().next_back().is_some_and(ident) {
            continue;
        }
        let rest = &API_HEADER[at..];
        let len = rest.find(|c: char| !ident(c)).unwrap_or(rest.len());
        if rest[len..].trim_start().starts_with('(') {
            names.insert(&rest[..len]);
        }
    }
    names
}

#[test]
fn every_exported_function_matches_its_binding() {
    let checked = assert_parity!(
        PDUConstruct,
        PDUDestruct,
        PDUGetVersion,
        PDUGetStatus,
        PDUGetLastError,
        PDUGetResourceStatus,
        PDUCreateComLogicalLink,
        PDUDestroyComLogicalLink,
        PDUConnect,
        PDUDisconnect,
        PDULockResource,
        PDUUnlockResource,
        PDUGetComParam,
        PDUSetComParam,
        PDUStartComPrimitive,
        PDUCancelComPrimitive,
        PDUGetEventItem,
        PDUDestroyItem,
        PDURegisterEventCallback,
        PDUGetObjectId,
        PDUGetModuleIds,
        PDUGetResourceIds,
        PDUGetConflictingResources,
        PDUGetUniqueRespIdTable,
        PDUSetUniqueRespIdTable,
        PDUModuleConnect,
        PDUModuleDisconnect,
        PDUGetTimestamp,
    );
    let listed: BTreeSet<&str> = checked.iter().copied().collect();
    assert_eq!(listed.len(), checked.len(), "a name is listed twice");
    // Every function the header declares is checked, `PDUIoCtl` by the next
    // test, so a new entry point cannot be left out.
    let mut covered = listed;
    covered.insert("PDUIoCtl");
    assert_eq!(covered, header_functions());
}

/// `PDUIoCtl` is the one export that differs from its binding: the header
/// (`crates/iso22900-sys/src/bindings/d_pdu_api_func.h`), and so the
/// bindings, declare the IOCTL command ID as `UNUM32`, while the mock takes
/// it as `T_PDU_IT`. Both are 32-bit integers (`T_PDU_IT` is a
/// `repr(transparent)` wrapper), so the call still passes the same value the
/// same way; this test pins that, until the mock's parameter type matches the
/// binding and `PDUIoCtl` can join the list above.
#[test]
fn pdu_ioctl_differs_only_in_a_same_layout_command_id_type() {
    use std::mem::{align_of, size_of};
    assert_eq!(size_of::<T_PDU_IT>(), size_of::<UNUM32>());
    assert_eq!(align_of::<T_PDU_IT>(), align_of::<UNUM32>());
}
