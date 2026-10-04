use std::borrow::{Borrow, ToOwned};

use iso22900_sys::bindings::T_PDU_PC;

use super::*;

impl BorrowedParamItem {
    /// Returns COM parameter identifier.
    pub fn id(&self) -> UNUM32 {
        self.0.ComParamId
    }

    /// Returns COM parameter data type.
    pub fn data_type(&self) -> ParamDataType {
        ParamDataType(self.0.ComParamDataType)
    }

    /// Returns COM parameter class value.
    pub fn class(&self) -> T_PDU_PC {
        self.0.ComParamClass
    }

    fn ensure_data_type(
        &self,
        expected: T_PDU_PT,
        payload_name: &'static str,
    ) -> Result<(), DPduApiError> {
        if self.0.ComParamDataType == expected {
            Ok(())
        } else {
            Err(DPduApiError::Unsupported(payload_name))
        }
    }

    /// Returns unsigned 8-bit parameter payload.
    pub fn unum8_data(&self) -> Result<&u8, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_UNUM8, "com parameter unum8 payload")?;
        self.data::<u8>()
    }

    /// Returns mutable unsigned 8-bit parameter payload.
    pub fn unum8_data_mut(&mut self) -> Result<&mut u8, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_UNUM8, "com parameter unum8 payload")?;
        self.data_mut::<u8>()
    }

    /// Returns signed 8-bit parameter payload.
    pub fn snum8_data(&self) -> Result<&i8, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_SNUM8, "com parameter snum8 payload")?;
        self.data::<i8>()
    }

    pub fn snum8_data_mut(&mut self) -> Result<&mut i8, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_SNUM8, "com parameter snum8 payload")?;
        self.data_mut::<i8>()
    }

    /// Returns unsigned 16-bit parameter payload.
    pub fn unum16_data(&self) -> Result<&u16, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_UNUM16, "com parameter unum16 payload")?;
        self.data::<u16>()
    }

    pub fn unum16_data_mut(&mut self) -> Result<&mut u16, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_UNUM16, "com parameter unum16 payload")?;
        self.data_mut::<u16>()
    }

    /// Returns signed 16-bit parameter payload.
    pub fn snum16_data(&self) -> Result<&i16, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_SNUM16, "com parameter snum16 payload")?;
        self.data::<i16>()
    }

    pub fn snum16_data_mut(&mut self) -> Result<&mut i16, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_SNUM16, "com parameter snum16 payload")?;
        self.data_mut::<i16>()
    }

    /// Returns unsigned 32-bit parameter payload.
    pub fn unum32_data(&self) -> Result<&u32, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_UNUM32, "com parameter unum32 payload")?;
        self.data::<u32>()
    }

    pub fn unum32_data_mut(&mut self) -> Result<&mut u32, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_UNUM32, "com parameter unum32 payload")?;
        self.data_mut::<u32>()
    }

    /// Returns signed 32-bit parameter payload.
    pub fn snum32_data(&self) -> Result<&i32, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_SNUM32, "com parameter snum32 payload")?;
        self.data::<i32>()
    }

    pub fn snum32_data_mut(&mut self) -> Result<&mut i32, DPduApiError> {
        self.ensure_data_type(E_PDU_PT::PDU_PT_SNUM32, "com parameter snum32 payload")?;
        self.data_mut::<i32>()
    }

    /// Interprets parameter payload pointer as T.
    pub fn data<T>(&self) -> Result<&T, DPduApiError> {
        let ptr = self.0.pComParamData.cast::<T>();
        unsafe { ptr.as_ref() }.ok_or(DPduApiError::NullPointer("com parameter data"))
    }

    pub fn data_mut<T>(&mut self) -> Result<&mut T, DPduApiError> {
        let ptr = self.0.pComParamData.cast::<T>();
        unsafe { ptr.as_mut() }.ok_or(DPduApiError::NullPointer("com parameter data"))
    }

    /// Returns bytefield payload when data type is bytefield.
    pub fn bytefield_data(&self) -> Result<&BorrowedBytefieldData, DPduApiError> {
        self.ensure_data_type(
            E_PDU_PT::PDU_PT_BYTEFIELD,
            "com parameter bytefield payload",
        )?;
        let ptr = self.0.pComParamData.cast::<PDU_PARAM_BYTEFIELD_DATA>();
        unsafe { ptr.as_ref() }
            .map(|d| unsafe {
                &*(d as *const PDU_PARAM_BYTEFIELD_DATA as *const BorrowedBytefieldData)
            })
            .ok_or(DPduApiError::NullPointer("com parameter bytefield value"))
    }

    pub fn bytefield_data_mut(&mut self) -> Result<&mut BorrowedBytefieldData, DPduApiError> {
        self.ensure_data_type(
            E_PDU_PT::PDU_PT_BYTEFIELD,
            "com parameter bytefield payload",
        )?;
        let ptr = self.0.pComParamData.cast::<PDU_PARAM_BYTEFIELD_DATA>();
        unsafe { ptr.as_mut() }
            .map(|d| unsafe {
                &mut *(d as *mut PDU_PARAM_BYTEFIELD_DATA as *mut BorrowedBytefieldData)
            })
            .ok_or(DPduApiError::NullPointer("com parameter bytefield value"))
    }

    /// Returns longfield payload when data type is longfield.
    pub fn longfield_data(&self) -> Result<&BorrowedLongfieldData, DPduApiError> {
        self.ensure_data_type(
            E_PDU_PT::PDU_PT_LONGFIELD,
            "com parameter longfield payload",
        )?;
        let ptr = self.0.pComParamData.cast::<PDU_PARAM_LONGFIELD_DATA>();
        unsafe { ptr.as_ref() }
            .map(|d| unsafe {
                &*(d as *const PDU_PARAM_LONGFIELD_DATA as *const BorrowedLongfieldData)
            })
            .ok_or(DPduApiError::NullPointer("com parameter longfield value"))
    }

    pub fn longfield_data_mut(&mut self) -> Result<&mut BorrowedLongfieldData, DPduApiError> {
        self.ensure_data_type(
            E_PDU_PT::PDU_PT_LONGFIELD,
            "com parameter longfield payload",
        )?;
        let ptr = self.0.pComParamData.cast::<PDU_PARAM_LONGFIELD_DATA>();
        unsafe { ptr.as_mut() }
            .map(|d| unsafe {
                &mut *(d as *mut PDU_PARAM_LONGFIELD_DATA as *mut BorrowedLongfieldData)
            })
            .ok_or(DPduApiError::NullPointer("com parameter longfield value"))
    }

    /// Returns structfield payload when data type is structfield.
    pub fn structfield_data(&self) -> Result<&BorrowedStructfieldData, DPduApiError> {
        self.ensure_data_type(
            E_PDU_PT::PDU_PT_STRUCTFIELD,
            "com parameter structfield payload",
        )?;
        let ptr = self.0.pComParamData.cast::<PDU_PARAM_STRUCTFIELD_DATA>();
        unsafe { ptr.as_ref() }
            .map(|d| unsafe {
                &*(d as *const PDU_PARAM_STRUCTFIELD_DATA as *const BorrowedStructfieldData)
            })
            .ok_or(DPduApiError::NullPointer("com parameter structfield value"))
    }

    pub fn structfield_data_mut(&mut self) -> Result<&mut BorrowedStructfieldData, DPduApiError> {
        self.ensure_data_type(
            E_PDU_PT::PDU_PT_STRUCTFIELD,
            "com parameter structfield payload",
        )?;
        let ptr = self.0.pComParamData.cast::<PDU_PARAM_STRUCTFIELD_DATA>();
        unsafe { ptr.as_mut() }
            .map(|d| unsafe {
                &mut *(d as *mut PDU_PARAM_STRUCTFIELD_DATA as *mut BorrowedStructfieldData)
            })
            .ok_or(DPduApiError::NullPointer("com parameter structfield value"))
    }

    /// Dispatches immutable parameter payload to a visitor.
    pub fn visit<V: ParamItemVisitor>(&self, visitor: &mut V) -> Result<V::Output, DPduApiError> {
        match self.0.ComParamDataType {
            E_PDU_PT::PDU_PT_UNUM8 => visitor.visit_unum8(self.unum8_data()?),
            E_PDU_PT::PDU_PT_SNUM8 => visitor.visit_snum8(self.snum8_data()?),
            E_PDU_PT::PDU_PT_UNUM16 => visitor.visit_unum16(self.unum16_data()?),
            E_PDU_PT::PDU_PT_SNUM16 => visitor.visit_snum16(self.snum16_data()?),
            E_PDU_PT::PDU_PT_UNUM32 => visitor.visit_unum32(self.unum32_data()?),
            E_PDU_PT::PDU_PT_SNUM32 => visitor.visit_snum32(self.snum32_data()?),
            E_PDU_PT::PDU_PT_BYTEFIELD => visitor.visit_bytefield(self.bytefield_data()?),
            E_PDU_PT::PDU_PT_STRUCTFIELD => visitor.visit_structfield(self.structfield_data()?),
            E_PDU_PT::PDU_PT_LONGFIELD => visitor.visit_longfield(self.longfield_data()?),
            _ => Err(DPduApiError::Unsupported("com parameter data type")),
        }
    }

    /// Dispatches mutable parameter payload to a visitor.
    pub fn visit_mut<V: ParamItemVisitorMut>(
        &mut self,
        visitor: &mut V,
    ) -> Result<V::Output, DPduApiError> {
        match self.0.ComParamDataType {
            E_PDU_PT::PDU_PT_UNUM8 => visitor.visit_unum8(self.unum8_data_mut()?),
            E_PDU_PT::PDU_PT_SNUM8 => visitor.visit_snum8(self.snum8_data_mut()?),
            E_PDU_PT::PDU_PT_UNUM16 => visitor.visit_unum16(self.unum16_data_mut()?),
            E_PDU_PT::PDU_PT_SNUM16 => visitor.visit_snum16(self.snum16_data_mut()?),
            E_PDU_PT::PDU_PT_UNUM32 => visitor.visit_unum32(self.unum32_data_mut()?),
            E_PDU_PT::PDU_PT_SNUM32 => visitor.visit_snum32(self.snum32_data_mut()?),
            E_PDU_PT::PDU_PT_BYTEFIELD => visitor.visit_bytefield(self.bytefield_data_mut()?),
            E_PDU_PT::PDU_PT_STRUCTFIELD => visitor.visit_structfield(self.structfield_data_mut()?),
            E_PDU_PT::PDU_PT_LONGFIELD => visitor.visit_longfield(self.longfield_data_mut()?),
            _ => Err(DPduApiError::Unsupported("com parameter data type")),
        }
    }

    /// Clones parameter into owned storage with explicit vendor struct entry size.
    ///
    /// # Safety
    ///
    /// `vendor_entry_size` is only consulted for a vendor (non-standard)
    /// `PDU_PT_STRUCTFIELD` with a nonzero entry count; when it is, the
    /// caller must supply the connected library's true per-entry byte
    /// layout for that struct type, from trusted (operator-configured)
    /// information -- never from client-controlled input, directly or
    /// indirectly (a value learned from a prior client write is NOT trusted
    /// information, since a successful native write never validates the
    /// caller's declared size against the DLL's real layout; ADR-218 as
    /// amended). An incorrect value is undefined behavior: this function has
    /// no way to check it against the native allocation's actual size,
    /// since the D-PDU API surfaces no self-describing metadata for a
    /// vendor struct type (Codex review, PR #133 ninth round).
    pub unsafe fn to_owned_with_vendor_entry_size(
        &self,
        vendor_entry_size: usize,
    ) -> Result<OwnedParamItem, DPduApiError> {
        let (raw, storage) =
            OwnedParamItem::from_borrowed_parts_with_vendor_entry_size(self, vendor_entry_size)?;
        Ok(OwnedParamItem {
            borrowed: BorrowedParamItem(raw),
            storage,
        })
    }
}

impl ParamStorage {
    fn as_mut_ptr(&mut self) -> *mut c_void {
        match self {
            Self::U8(value) => (&mut **value as *mut u8).cast(),
            Self::I8(value) => (&mut **value as *mut i8).cast(),
            Self::U16(value) => (&mut **value as *mut u16).cast(),
            Self::I16(value) => (&mut **value as *mut i16).cast(),
            Self::U32(value) => (&mut **value as *mut u32).cast(),
            Self::I32(value) => (&mut **value as *mut i32).cast(),
            Self::ByteField { meta, .. } => (&mut **meta as *mut PDU_PARAM_BYTEFIELD_DATA).cast(),
            Self::LongField { meta, .. } => (&mut **meta as *mut PDU_PARAM_LONGFIELD_DATA).cast(),
            Self::StructField { meta, .. } => {
                (&mut **meta as *mut PDU_PARAM_STRUCTFIELD_DATA).cast()
            }
            Self::Unsupported => std::ptr::null_mut(),
        }
    }

    #[inline(never)]
    pub(crate) fn keep_alive(&self) {
        match self {
            Self::U8(value) => {
                std::hint::black_box(&**value as *const u8);
            }
            Self::I8(value) => {
                std::hint::black_box(&**value as *const i8);
            }
            Self::U16(value) => {
                std::hint::black_box(&**value as *const u16);
            }
            Self::I16(value) => {
                std::hint::black_box(&**value as *const i16);
            }
            Self::U32(value) => {
                std::hint::black_box(&**value as *const u32);
            }
            Self::I32(value) => {
                std::hint::black_box(&**value as *const i32);
            }
            Self::ByteField { meta, data } => {
                std::hint::black_box(&**meta as *const PDU_PARAM_BYTEFIELD_DATA);
                std::hint::black_box(data.as_ptr());
                std::hint::black_box(data.len());
            }
            Self::LongField { meta, data } => {
                std::hint::black_box(&**meta as *const PDU_PARAM_LONGFIELD_DATA);
                std::hint::black_box(data.as_ptr());
                std::hint::black_box(data.len());
            }
            Self::StructField { meta, bytes } => {
                std::hint::black_box(&**meta as *const PDU_PARAM_STRUCTFIELD_DATA);
                std::hint::black_box(bytes.as_ptr());
                std::hint::black_box(bytes.len());
            }
            Self::Unsupported => {}
        }
    }
}

impl OwnedParamItem {
    /// `vendor_entry_size`, when `Some`, has an uncheckable precondition:
    /// the caller must supply the connected library's true per-entry byte
    /// layout for `item`'s vendor `ComParamStructType`, from trusted
    /// (operator-configured) information -- never from client-controlled
    /// input, directly or indirectly (e.g. a value learned from a prior
    /// client write is NOT trusted information, since a successful native
    /// write never validates the caller's declared size against the DLL's
    /// real layout; ADR-218 as amended). An incorrect value is undefined
    /// behavior at the lower-level slice construction this eventually calls
    /// (`BorrowedStructfieldData::struct_array_bytes_with_entry_size`).
    fn from_borrowed_parts_impl(
        item: &BorrowedParamItem,
        vendor_entry_size: Option<usize>,
    ) -> Result<(PDU_PARAM_ITEM, ParamStorage), DPduApiError> {
        let mut storage = match item.0.ComParamDataType {
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_UNUM8 => {
                ParamStorage::U8(Box::new(*item.unum8_data()?))
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_SNUM8 => {
                ParamStorage::I8(Box::new(*item.snum8_data()?))
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_UNUM16 => {
                ParamStorage::U16(Box::new(*item.unum16_data()?))
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_SNUM16 => {
                ParamStorage::I16(Box::new(*item.snum16_data()?))
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_UNUM32 => {
                ParamStorage::U32(Box::new(*item.unum32_data()?))
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_SNUM32 => {
                ParamStorage::I32(Box::new(*item.snum32_data()?))
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_BYTEFIELD => {
                let data = item.bytefield_data()?;
                let mut bytes = data.data_array()?.to_vec();
                let mut meta = Box::new(PDU_PARAM_BYTEFIELD_DATA {
                    ParamMaxLen: data.param_max_len(),
                    ParamActLen: data.param_actual_len(),
                    pDataArray: std::ptr::null_mut(),
                });
                meta.pDataArray = vec_ptr_or_null(&mut bytes);
                ParamStorage::ByteField { meta, data: bytes }
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_LONGFIELD => {
                let data = item.longfield_data()?;
                let mut longs = data.data_array()?.to_vec();
                let mut meta = Box::new(PDU_PARAM_LONGFIELD_DATA {
                    ParamMaxLen: data.param_max_len(),
                    ParamActLen: data.param_actual_len(),
                    pDataArray: std::ptr::null_mut(),
                });
                meta.pDataArray = vec_ptr_or_null(&mut longs);
                ParamStorage::LongField { meta, data: longs }
            }
            iso22900_sys::bindings::E_PDU_PT::PDU_PT_STRUCTFIELD => {
                let data = item.structfield_data()?;
                let mut bytes = match data.param_struct_type() {
                    E_PDU_CPST::PDU_CPST_SESSION_TIMING
                    | E_PDU_CPST::PDU_CPST_ACCESS_TIMING
                    | E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER => {
                        data.struct_array_bytes()?.to_vec()
                    }
                    _ => {
                        let size = vendor_entry_size.ok_or(DPduApiError::Unsupported(
                            "vendor specific structfield clone requires explicit entry size",
                        ))?;
                        // Safety: `vendor_entry_size` is an uncheckable
                        // precondition of this function -- the caller must
                        // supply the connected library's true per-entry byte
                        // layout from trusted (operator-configured)
                        // information, never from client-controlled input.
                        // See `BorrowedStructfieldData::struct_array_bytes_with_entry_size`'s
                        // `# Safety` section (ADR-218 as amended).
                        unsafe { data.struct_array_bytes_with_entry_size(size) }?.to_vec()
                    }
                };

                let mut meta = Box::new(PDU_PARAM_STRUCTFIELD_DATA {
                    ComParamStructType: data.param_struct_type(),
                    ParamMaxEntries: data.param_max_entries(),
                    ParamActEntries: data.param_actual_entries(),
                    pStructArray: std::ptr::null_mut(),
                });
                meta.pStructArray = vec_ptr_or_null(&mut bytes).cast();
                ParamStorage::StructField { meta, bytes }
            }
            _ => ParamStorage::Unsupported,
        };

        let mut raw = item.0;
        raw.pComParamData = storage.as_mut_ptr();
        Ok((raw, storage))
    }

    pub(crate) fn from_borrowed_parts(
        item: &BorrowedParamItem,
    ) -> Result<(PDU_PARAM_ITEM, ParamStorage), DPduApiError> {
        Self::from_borrowed_parts_impl(item, None)
    }

    /// See [`Self::from_borrowed_parts_impl`]'s doc comment for
    /// `vendor_entry_size`'s uncheckable precondition.
    pub(crate) fn from_borrowed_parts_with_vendor_entry_size(
        item: &BorrowedParamItem,
        vendor_entry_size: usize,
    ) -> Result<(PDU_PARAM_ITEM, ParamStorage), DPduApiError> {
        Self::from_borrowed_parts_impl(item, Some(vendor_entry_size))
    }

    pub(crate) fn from_borrowed(item: &BorrowedParamItem) -> Result<Self, DPduApiError> {
        let (raw, storage) = Self::from_borrowed_parts(item)?;
        Ok(Self {
            borrowed: BorrowedParamItem(raw),
            storage,
        })
    }
}

impl Borrow<BorrowedParamItem> for OwnedParamItem {
    fn borrow(&self) -> &BorrowedParamItem {
        self.storage.keep_alive();
        &self.borrowed
    }
}

impl ToOwned for BorrowedParamItem {
    type Owned = OwnedParamItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedParamItem::from_borrowed(self).expect("com parameter item is not cloneable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_value<T>(value: T) -> T {
        value
    }

    fn vendor_struct_type(raw: u32) -> T_PDU_CPST {
        unsafe { std::mem::transmute_copy::<u32, T_PDU_CPST>(&raw) }
    }

    #[test]
    fn unum32_data_rejects_wrong_type() {
        let raw = PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 0,
            ComParamDataType: E_PDU_PT::PDU_PT_UNUM8,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: std::ptr::null_mut(),
        };
        let item = BorrowedParamItem(raw);

        let err = item.unum32_data().expect_err("expected type mismatch");
        match err {
            DPduApiError::Unsupported(name) => assert_eq!(name, "com parameter unum32 payload"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn unum32_data_detects_null_payload() {
        let raw = PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 0,
            ComParamDataType: E_PDU_PT::PDU_PT_UNUM32,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: std::ptr::null_mut(),
        };
        let item = BorrowedParamItem(raw);

        let err = item.unum32_data().expect_err("expected null payload error");
        match err {
            DPduApiError::NullPointer(name) => assert_eq!(name, "com parameter data"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn owned_param_item_keeps_data_pointer_after_move() {
        let mut value = 0x1122_3344_u32;
        let borrowed = BorrowedParamItem(PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 1,
            ComParamDataType: E_PDU_PT::PDU_PT_UNUM32,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: (&mut value as *mut u32).cast(),
        });

        let owned = OwnedParamItem::from_borrowed(&borrowed).expect("clone should succeed");
        let ptr_before = owned.borrowed.0.pComParamData.cast::<u32>();
        assert!(!ptr_before.is_null());
        assert_eq!(unsafe { *ptr_before }, 0x1122_3344);

        let owned = move_value(owned);
        let ptr_after = owned.borrowed.0.pComParamData.cast::<u32>();
        assert_eq!(ptr_after, ptr_before);
        assert_eq!(unsafe { *ptr_after }, 0x1122_3344);
    }

    #[test]
    fn vendor_structfield_clone_requires_entry_size() {
        let mut vendor_payload = vec![0xAA_u8, 0xBB, 0xCC, 0xDD];
        let mut structfield = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0001),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload.as_mut_ptr().cast(),
        };

        let borrowed = BorrowedParamItem(PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 1,
            ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: (&mut structfield as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
        });

        let err = OwnedParamItem::from_borrowed(&borrowed)
            .expect_err("vendor structfield clone should require entry size");
        match err {
            DPduApiError::Unsupported(name) => {
                assert_eq!(
                    name,
                    "vendor specific structfield clone requires explicit entry size"
                )
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn vendor_structfield_clone_preserves_payload_with_entry_size() {
        let mut vendor_payload = vec![0x10_u8, 0x20, 0x30, 0x40];
        let mut structfield = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0001),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload.as_mut_ptr().cast(),
        };

        let borrowed = BorrowedParamItem(PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 7,
            ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: (&mut structfield as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
        });

        let (raw, storage) =
            OwnedParamItem::from_borrowed_parts_with_vendor_entry_size(&borrowed, 2)
                .expect("vendor structfield clone should succeed with entry size");

        let raw_meta = unsafe { &*raw.pComParamData.cast::<PDU_PARAM_STRUCTFIELD_DATA>() };
        assert_eq!(
            raw_meta.ComParamStructType,
            E_PDU_CPST(0x8000_0001_u32 as _)
        );
        assert_eq!(raw_meta.ParamActEntries, 2);
        assert!(!raw_meta.pStructArray.is_null());

        match storage {
            ParamStorage::StructField { bytes, .. } => {
                assert_eq!(bytes, vec![0x10, 0x20, 0x30, 0x40])
            }
            other => panic!("unexpected storage: {other:?}"),
        }
    }
}
