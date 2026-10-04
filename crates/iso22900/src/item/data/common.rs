use super::*;
use std::ffi::c_void;

/// Resource status entry.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedResourceStatusData(pub PDU_RSC_STATUS_DATA);

impl BorrowedResourceStatusData {
    /// Returns module handle.
    pub fn module_handle(&self) -> ModuleHandle {
        ModuleHandle(self.0.hMod)
    }

    /// Returns resource identifier.
    pub fn resource_id(&self) -> ResourceId {
        ResourceId(self.0.ResourceId)
    }

    /// Returns resource status code.
    pub fn resource_status(&self) -> UNUM32 {
        self.0.ResourceStatus
    }
}

/// Resource conflict entry.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedResourceConflictData(pub PDU_RSC_CONFLICT_DATA);

impl BorrowedResourceConflictData {
    /// Wraps raw conflict data.
    pub fn from_raw(raw: PDU_RSC_CONFLICT_DATA) -> Self {
        Self(raw)
    }

    /// Returns module handle.
    pub fn module_handle(&self) -> ModuleHandle {
        ModuleHandle(self.0.hMod)
    }

    /// Returns conflicting resource identifier.
    pub fn resource_id(&self) -> ResourceId {
        ResourceId(self.0.ResourceId)
    }
}

/// Resource-id entry per module.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedResourceIdData(pub PDU_RSC_ID_ITEM_DATA);

impl BorrowedResourceIdData {
    /// Wraps raw resource-id entry.
    pub fn from_raw(raw: PDU_RSC_ID_ITEM_DATA) -> Self {
        Self(raw)
    }

    /// Returns module handle.
    pub fn module_handle(&self) -> UNUM32 {
        self.0.hMod
    }

    /// Returns immutable resource-id list.
    pub fn resource_ids(&self) -> Result<&[UNUM32], DPduApiError> {
        unsafe { ffi_slice(self.0.pResourceIdArray, self.0.NumIds, "resource id array") }
    }

    /// Returns mutable resource-id list.
    pub fn resource_ids_mut(&mut self) -> Result<&mut [UNUM32], DPduApiError> {
        unsafe { ffi_slice_mut(self.0.pResourceIdArray, self.0.NumIds, "resource id array") }
    }
}

/// Bytefield parameter payload.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedBytefieldData(pub PDU_PARAM_BYTEFIELD_DATA);

impl BorrowedBytefieldData {
    /// Wraps raw bytefield data.
    pub fn from_raw(raw: PDU_PARAM_BYTEFIELD_DATA) -> Self {
        Self(raw)
    }

    /// Returns maximum payload length.
    pub fn param_max_len(&self) -> UNUM32 {
        self.0.ParamMaxLen
    }

    /// Returns actual payload length.
    pub fn param_actual_len(&self) -> UNUM32 {
        self.0.ParamActLen
    }

    /// Returns immutable bytefield payload bytes.
    pub fn data_array(&self) -> Result<&[u8], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pDataArray.cast::<u8>(),
                self.0.ParamActLen,
                "bytefield data",
            )
        }
    }

    /// Returns mutable bytefield payload bytes.
    pub fn data_array_mut(&mut self) -> Result<&mut [u8], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pDataArray.cast::<u8>(),
                self.0.ParamActLen,
                "bytefield data array",
            )
        }
    }
}

/// Longfield parameter payload.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedLongfieldData(pub PDU_PARAM_LONGFIELD_DATA);

impl BorrowedLongfieldData {
    /// Wraps raw longfield data.
    pub fn from_raw(raw: PDU_PARAM_LONGFIELD_DATA) -> Self {
        Self(raw)
    }

    /// Returns maximum payload length.
    pub fn param_max_len(&self) -> UNUM32 {
        self.0.ParamMaxLen
    }

    /// Returns actual payload length.
    pub fn param_actual_len(&self) -> UNUM32 {
        self.0.ParamActLen
    }

    /// Returns immutable longfield payload values.
    pub fn data_array(&self) -> Result<&[UNUM32], DPduApiError> {
        unsafe { ffi_slice(self.0.pDataArray, self.0.ParamActLen, "longfield data") }
    }

    /// Returns mutable longfield payload values.
    pub fn data_array_mut(&mut self) -> Result<&mut [UNUM32], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pDataArray,
                self.0.ParamActLen,
                "longfield data array",
            )
        }
    }
}

/// Structfield parameter payload.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedStructfieldData(pub PDU_PARAM_STRUCTFIELD_DATA);

#[derive(Debug, Clone, Copy)]
pub enum StructfieldStructArrayRef<'a> {
    SessionTiming(&'a [PDU_PARAM_STRUCT_SESS_TIMING]),
    AccessTiming(&'a [PDU_PARAM_STRUCT_ACCESS_TIMING]),
    TlsVersionAndCipher(&'a [PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER]),
    VendorSpecific(BorrowedVendorSpecificStructArray<'a>),
}

#[derive(Debug, Clone, Copy)]
pub struct BorrowedVendorSpecificStructArray<'a> {
    struct_type: E_PDU_CPST,
    entries: UNUM32,
    ptr: *const c_void,
    _marker: std::marker::PhantomData<&'a c_void>,
}

impl<'a> BorrowedVendorSpecificStructArray<'a> {
    /// Returns vendor struct type code.
    pub fn struct_type(&self) -> E_PDU_CPST {
        self.struct_type
    }

    /// Returns number of entries.
    pub fn entries(&self) -> UNUM32 {
        self.entries
    }

    /// Returns raw pointer to struct array data.
    pub fn as_ptr(&self) -> *const c_void {
        self.ptr
    }

    /// Returns payload bytes using explicit vendor entry size.
    ///
    /// # Safety
    ///
    /// The native D-PDU API exposes no way to recover a vendor struct type's
    /// true per-entry byte size from the call itself -- `entry_size` must
    /// come from trusted, operator-configured information about the
    /// connected library's real in-memory layout for this struct type (see
    /// `iso22900-service`'s `vendor_struct_types` config, ADR-218 as amended
    /// to remove the vulnerable client-write-learned size source). A
    /// successful native `PDUSetComParam` never validates a caller-declared
    /// entry size against the DLL's actual layout, so `entry_size` supplied
    /// from anything the client controls (directly or indirectly, e.g. a
    /// value cached from a prior client write) is untrustworthy: an
    /// incorrect value causes this function to construct a slice extending
    /// past the native allocation's true length, which the D-PDU API never
    /// exposes -- undefined behavior.
    pub unsafe fn bytes(&self, entry_size: usize) -> Result<&'a [u8], DPduApiError> {
        if entry_size == 0 {
            return Err(DPduApiError::Unsupported(
                "vendor specific structfield entry size must be greater than zero",
            ));
        }
        let byte_len =
            (self.entries as usize)
                .checked_mul(entry_size)
                .ok_or(DPduApiError::Unsupported(
                    "vendor specific structfield byte length overflow",
                ))?;
        let len_unum32 = UNUM32::try_from(byte_len).map_err(|_| {
            DPduApiError::Unsupported(
                "vendor specific structfield byte length exceeds UNUM32 range",
            )
        })?;
        unsafe {
            ffi_slice(
                self.ptr.cast::<u8>(),
                len_unum32,
                "vendor specific structfield data",
            )
        }
    }
}

/// Visitor for typed structfield array payload variants.
pub trait StructfieldStructArrayVisitor {
    type Output;

    fn visit_session_timing(
        &mut self,
        data: &[PDU_PARAM_STRUCT_SESS_TIMING],
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_access_timing(
        &mut self,
        data: &[PDU_PARAM_STRUCT_ACCESS_TIMING],
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_tls_version_and_cipher(
        &mut self,
        data: &[PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER],
    ) -> Result<Self::Output, DPduApiError>;
    fn visit_vendor_specific(
        &mut self,
        data: BorrowedVendorSpecificStructArray<'_>,
    ) -> Result<Self::Output, DPduApiError>;
}

impl BorrowedStructfieldData {
    /// Wraps raw structfield data.
    pub fn from_raw(raw: PDU_PARAM_STRUCTFIELD_DATA) -> Self {
        Self(raw)
    }

    /// Returns struct type.
    pub fn param_struct_type(&self) -> T_PDU_CPST {
        self.0.ComParamStructType
    }

    /// Returns maximum number of entries.
    pub fn param_max_entries(&self) -> UNUM32 {
        self.0.ParamMaxEntries
    }

    /// Returns actual number of entries.
    pub fn param_actual_entries(&self) -> UNUM32 {
        self.0.ParamActEntries
    }

    fn ensure_struct_type(
        &self,
        expected: T_PDU_CPST,
        payload_name: &'static str,
    ) -> Result<(), DPduApiError> {
        if self.0.ComParamStructType == expected {
            Ok(())
        } else {
            Err(DPduApiError::Unsupported(payload_name))
        }
    }

    fn struct_array_as<T>(
        &self,
        expected: T_PDU_CPST,
        payload_name: &'static str,
        null_name: &'static str,
    ) -> Result<&[T], DPduApiError> {
        self.ensure_struct_type(expected, payload_name)?;
        unsafe {
            ffi_slice(
                self.0.pStructArray.cast::<T>(),
                self.0.ParamActEntries,
                null_name,
            )
        }
    }

    /// Returns typed view of struct array data.
    pub fn struct_array(&self) -> Result<StructfieldStructArrayRef<'_>, DPduApiError> {
        match self.0.ComParamStructType {
            E_PDU_CPST::PDU_CPST_SESSION_TIMING => Ok(StructfieldStructArrayRef::SessionTiming(
                self.session_timing_structs()?,
            )),
            E_PDU_CPST::PDU_CPST_ACCESS_TIMING => Ok(StructfieldStructArrayRef::AccessTiming(
                self.access_timing_structs()?,
            )),
            E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER => Ok(
                StructfieldStructArrayRef::TlsVersionAndCipher(self.tls_version_cipher_structs()?),
            ),
            _ => Ok(StructfieldStructArrayRef::VendorSpecific(
                BorrowedVendorSpecificStructArray {
                    struct_type: self.0.ComParamStructType,
                    entries: self.0.ParamActEntries,
                    ptr: self.0.pStructArray.cast_const().cast(),
                    _marker: std::marker::PhantomData,
                },
            )),
        }
    }

    /// Dispatches struct array variant to a visitor.
    pub fn visit_struct_array<V: StructfieldStructArrayVisitor>(
        &self,
        visitor: &mut V,
    ) -> Result<V::Output, DPduApiError> {
        match self.struct_array()? {
            StructfieldStructArrayRef::SessionTiming(data) => visitor.visit_session_timing(data),
            StructfieldStructArrayRef::AccessTiming(data) => visitor.visit_access_timing(data),
            StructfieldStructArrayRef::TlsVersionAndCipher(data) => {
                visitor.visit_tls_version_and_cipher(data)
            }
            StructfieldStructArrayRef::VendorSpecific(data) => visitor.visit_vendor_specific(data),
        }
    }

    /// Returns struct array bytes for standard struct types.
    pub fn struct_array_bytes(&self) -> Result<&[u8], DPduApiError> {
        match self.struct_array()? {
            StructfieldStructArrayRef::SessionTiming(data) => {
                let byte_len = std::mem::size_of_val(data);
                Ok(unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), byte_len) })
            }
            StructfieldStructArrayRef::AccessTiming(data) => {
                let byte_len = std::mem::size_of_val(data);
                Ok(unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), byte_len) })
            }
            StructfieldStructArrayRef::TlsVersionAndCipher(data) => {
                let byte_len = std::mem::size_of_val(data);
                Ok(unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), byte_len) })
            }
            StructfieldStructArrayRef::VendorSpecific(_) => Err(DPduApiError::Unsupported(
                "vendor specific structfield requires explicit entry size",
            )),
        }
    }

    /// Returns struct array bytes with explicit vendor entry size.
    ///
    /// # Safety
    ///
    /// For a vendor struct type, this defers to
    /// [`BorrowedVendorSpecificStructArray::bytes`] -- see that function's
    /// `# Safety` section for `vendor_entry_size`'s contract. Callers that
    /// know `self` holds a standard struct type may call this safely in
    /// practice (the vendor branch is never reached), but the standard-type
    /// case has no need for this function at all -- prefer
    /// [`Self::struct_array_bytes`] there, which needs no entry size and no
    /// `unsafe`.
    pub unsafe fn struct_array_bytes_with_entry_size(
        &self,
        vendor_entry_size: usize,
    ) -> Result<&[u8], DPduApiError> {
        match self.struct_array()? {
            StructfieldStructArrayRef::SessionTiming(_)
            | StructfieldStructArrayRef::AccessTiming(_)
            | StructfieldStructArrayRef::TlsVersionAndCipher(_) => self.struct_array_bytes(),
            StructfieldStructArrayRef::VendorSpecific(data) => unsafe {
                data.bytes(vendor_entry_size)
            },
        }
    }

    /// Returns session-timing entries.
    pub fn session_timing_structs(&self) -> Result<&[PDU_PARAM_STRUCT_SESS_TIMING], DPduApiError> {
        self.struct_array_as(
            E_PDU_CPST::PDU_CPST_SESSION_TIMING,
            "session timing structfield payload",
            "session timing structs",
        )
    }

    /// Returns access-timing entries.
    pub fn access_timing_structs(&self) -> Result<&[PDU_PARAM_STRUCT_ACCESS_TIMING], DPduApiError> {
        self.struct_array_as(
            E_PDU_CPST::PDU_CPST_ACCESS_TIMING,
            "access timing structfield payload",
            "access timing structs",
        )
    }

    /// Returns TLS version/cipher entries.
    pub fn tls_version_cipher_structs(
        &self,
    ) -> Result<&[PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER], DPduApiError> {
        self.struct_array_as(
            E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER,
            "tls version and cipher structfield payload",
            "tls version and cipher structs",
        )
    }
}
