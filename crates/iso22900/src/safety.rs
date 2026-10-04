use iso22900_sys::bindings::{T_PDU_ERROR, UNUM32};

use crate::DPduApiError;

#[inline]
pub(crate) fn check(code: T_PDU_ERROR) -> Result<(), DPduApiError> {
    if code == T_PDU_ERROR::PDU_STATUS_NOERROR {
        Ok(())
    } else {
        Err(DPduApiError::PduError(code.0 as UNUM32))
    }
}

pub(crate) unsafe fn ffi_slice<'a, T>(
    ptr: *const T,
    len: UNUM32,
    name: &'static str,
) -> Result<&'a [T], DPduApiError> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(DPduApiError::NullPointer(name));
    }
    Ok(unsafe { std::slice::from_raw_parts(ptr, len as usize) })
}

pub(crate) unsafe fn ffi_slice_mut<'a, T>(
    ptr: *mut T,
    len: UNUM32,
    name: &'static str,
) -> Result<&'a mut [T], DPduApiError> {
    if len == 0 {
        return Ok(unsafe {
            std::slice::from_raw_parts_mut(std::ptr::NonNull::<T>::dangling().as_ptr(), 0)
        });
    }
    if ptr.is_null() {
        return Err(DPduApiError::NullPointer(name));
    }
    Ok(unsafe { std::slice::from_raw_parts_mut(ptr, len as usize) })
}
