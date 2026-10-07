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

macro_rules! assert_parity {
    ($($name:ident),* $(,)?) => {{
        $(coerces_to(field_type(|api| api.$name), iso22900_mock::$name);)*
        [$(stringify!($name)),*].len()
    }};
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
    // Every entry point of the bindings' `DPduApiSys` is listed above, except
    // `PDUIoCtl` (next test).
    assert_eq!(checked, 28);
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
