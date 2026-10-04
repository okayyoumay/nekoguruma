use std::borrow::{Borrow, ToOwned};

use super::*;

impl BorrowedEcuUniqueRespData {
    /// Wraps raw ECU unique-response entry data.
    pub fn from_raw(raw: PDU_ECU_UNIQUE_RESP_DATA) -> Self {
        Self(raw)
    }

    /// Returns unique response identifier.
    pub fn unique_resp_identifier(&self) -> UNUM32 {
        self.0.UniqueRespIdentifier
    }

    pub(crate) fn params_raw(&self) -> Result<&[PDU_PARAM_ITEM], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pParams,
                self.0.NumParamItems,
                "unique response params",
            )
        }
    }

    /// Returns immutable COM parameter entries for this unique response.
    pub fn params(&self) -> Result<&[BorrowedParamItem], DPduApiError> {
        let raw_slice = self.params_raw()?;
        // Safety: BorrowedParamItem is #[repr(transparent)] over PDU_PARAM_ITEM
        Ok(unsafe {
            std::slice::from_raw_parts(
                raw_slice.as_ptr() as *const BorrowedParamItem,
                raw_slice.len(),
            )
        })
    }

    /// Returns mutable COM parameter entries for this unique response.
    pub fn params_mut(&mut self) -> Result<&mut [BorrowedParamItem], DPduApiError> {
        // Safety: BorrowedParamItem is #[repr(transparent)] over PDU_PARAM_ITEM
        unsafe {
            ffi_slice_mut(
                self.0.pParams as *mut BorrowedParamItem,
                self.0.NumParamItems,
                "unique response params array",
            )
        }
    }
}

impl BorrowedUniqueRespIdTableItem {
    pub(crate) fn entries_raw(&self) -> Result<&[PDU_ECU_UNIQUE_RESP_DATA], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pUniqueData,
                self.0.NumEntries,
                "unique response data",
            )
        }
    }

    /// Returns immutable unique-response entries.
    pub fn entries(&self) -> Result<&[BorrowedEcuUniqueRespData], DPduApiError> {
        let raw = self.entries_raw()?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedEcuUniqueRespData, raw.len())
        })
    }

    /// Returns mutable unique-response entries.
    pub fn entries_mut(&mut self) -> Result<&mut [BorrowedEcuUniqueRespData], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pUniqueData as *mut BorrowedEcuUniqueRespData,
                self.0.NumEntries,
                "unique response data array",
            )
        }
    }

    /// Clones to owned storage using a single vendor struct entry size.
    ///
    /// # Safety
    ///
    /// `vendor_entry_size` has the same uncheckable precondition as
    /// [`Self::to_owned_with_vendor_entry_size_resolver`]'s resolver
    /// closure -- see that function's `# Safety` section.
    pub unsafe fn to_owned_with_vendor_entry_size(
        &self,
        vendor_entry_size: usize,
    ) -> Result<OwnedUniqueRespIdTableItem, DPduApiError> {
        OwnedUniqueRespIdTableItem::from_borrowed_with_vendor_entry_size(self, vendor_entry_size)
    }

    /// Clones to owned storage using a per-struct-type size resolver.
    ///
    /// # Safety
    ///
    /// `vendor_entry_size_resolver` has an uncheckable precondition: for any
    /// struct type it resolves `Some(size)` for, `size` must be the
    /// connected library's true per-entry byte layout for that struct type,
    /// from trusted (operator-configured) information -- never from
    /// client-controlled input, directly or indirectly (a value learned
    /// from a prior client write is NOT trusted information, since a
    /// successful native write never validates the caller's declared size
    /// against the DLL's real layout; ADR-218 as amended). An incorrect
    /// value is undefined behavior at the lower-level slice construction
    /// this eventually calls: this function has no way to check it against
    /// the native allocation's actual size, since the D-PDU API surfaces no
    /// self-describing metadata for a vendor struct type (Codex review, PR
    /// #133 ninth round).
    pub unsafe fn to_owned_with_vendor_entry_size_resolver<F>(
        &self,
        vendor_entry_size_resolver: F,
    ) -> Result<OwnedUniqueRespIdTableItem, DPduApiError>
    where
        F: FnMut(T_PDU_CPST) -> Option<usize>,
    {
        OwnedUniqueRespIdTableItem::from_borrowed_with_vendor_entry_size_resolver(
            self,
            vendor_entry_size_resolver,
        )
    }
}

impl OwnedUniqueRespIdTableItem {
    fn is_standard_struct_type(struct_type: T_PDU_CPST) -> bool {
        matches!(
            struct_type,
            E_PDU_CPST::PDU_CPST_SESSION_TIMING
                | E_PDU_CPST::PDU_CPST_ACCESS_TIMING
                | E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER
        )
    }

    fn has_multiple_vendor_struct_types(
        item: &BorrowedUniqueRespIdTableItem,
    ) -> Result<bool, DPduApiError> {
        let entries = item.entries_raw()?;
        let mut first_vendor_type: Option<T_PDU_CPST> = None;
        for entry in entries {
            let params =
                unsafe { ffi_slice(entry.pParams, entry.NumParamItems, "unique response params") }?;
            for param in params {
                if param.ComParamDataType != E_PDU_PT::PDU_PT_STRUCTFIELD {
                    continue;
                }
                let borrowed_param = transparent_ref::<PDU_PARAM_ITEM, BorrowedParamItem>(param);
                let struct_type = borrowed_param.structfield_data()?.param_struct_type();
                if Self::is_standard_struct_type(struct_type) {
                    continue;
                }
                match first_vendor_type {
                    None => first_vendor_type = Some(struct_type),
                    Some(first) if first == struct_type => {}
                    Some(_) => return Ok(true),
                }
            }
        }
        Ok(false)
    }

    fn from_borrowed_with_vendor_entry_size_resolver<F>(
        item: &BorrowedUniqueRespIdTableItem,
        mut vendor_entry_size_resolver: F,
    ) -> Result<Self, DPduApiError>
    where
        F: FnMut(T_PDU_CPST) -> Option<usize>,
    {
        let entries = item.entries_raw()?;
        let total_params = entries
            .iter()
            .map(|entry| entry.NumParamItems as usize)
            .sum::<usize>();

        let mut scalar_u8_count = 0_usize;
        let mut scalar_i8_count = 0_usize;
        let mut scalar_u16_count = 0_usize;
        let mut scalar_i16_count = 0_usize;
        let mut scalar_u32_count = 0_usize;
        let mut scalar_i32_count = 0_usize;

        for entry in entries {
            let params =
                unsafe { ffi_slice(entry.pParams, entry.NumParamItems, "unique response params") }?;
            for param in params {
                match param.ComParamDataType {
                    E_PDU_PT::PDU_PT_UNUM8 => scalar_u8_count += 1,
                    E_PDU_PT::PDU_PT_SNUM8 => scalar_i8_count += 1,
                    E_PDU_PT::PDU_PT_UNUM16 => scalar_u16_count += 1,
                    E_PDU_PT::PDU_PT_SNUM16 => scalar_i16_count += 1,
                    E_PDU_PT::PDU_PT_UNUM32 => scalar_u32_count += 1,
                    E_PDU_PT::PDU_PT_SNUM32 => scalar_i32_count += 1,
                    _ => {}
                }
            }
        }

        let mut unique_data = Vec::with_capacity(entries.len());
        let mut param_items = Vec::with_capacity(total_params);
        let mut param_storages = Vec::with_capacity(total_params);
        let mut scalar_u8_data = Vec::with_capacity(scalar_u8_count);
        let mut scalar_i8_data = Vec::with_capacity(scalar_i8_count);
        let mut scalar_u16_data = Vec::with_capacity(scalar_u16_count);
        let mut scalar_i16_data = Vec::with_capacity(scalar_i16_count);
        let mut scalar_u32_data = Vec::with_capacity(scalar_u32_count);
        let mut scalar_i32_data = Vec::with_capacity(scalar_i32_count);

        for entry in entries {
            let params =
                unsafe { ffi_slice(entry.pParams, entry.NumParamItems, "unique response params") }?;
            let start_index = param_items.len();

            for param in params {
                let borrowed_param = transparent_ref::<PDU_PARAM_ITEM, BorrowedParamItem>(param);
                let mut raw_param = *param;
                match raw_param.ComParamDataType {
                    E_PDU_PT::PDU_PT_UNUM8 => {
                        scalar_u8_data.push(*borrowed_param.unum8_data()?);
                        raw_param.pComParamData =
                            scalar_u8_data.last_mut().expect("scalar arena reserved") as *mut u8
                                as *mut c_void;
                    }
                    E_PDU_PT::PDU_PT_SNUM8 => {
                        scalar_i8_data.push(*borrowed_param.snum8_data()?);
                        raw_param.pComParamData =
                            scalar_i8_data.last_mut().expect("scalar arena reserved") as *mut i8
                                as *mut c_void;
                    }
                    E_PDU_PT::PDU_PT_UNUM16 => {
                        scalar_u16_data.push(*borrowed_param.unum16_data()?);
                        raw_param.pComParamData =
                            scalar_u16_data.last_mut().expect("scalar arena reserved") as *mut u16
                                as *mut c_void;
                    }
                    E_PDU_PT::PDU_PT_SNUM16 => {
                        scalar_i16_data.push(*borrowed_param.snum16_data()?);
                        raw_param.pComParamData =
                            scalar_i16_data.last_mut().expect("scalar arena reserved") as *mut i16
                                as *mut c_void;
                    }
                    E_PDU_PT::PDU_PT_UNUM32 => {
                        scalar_u32_data.push(*borrowed_param.unum32_data()?);
                        raw_param.pComParamData =
                            scalar_u32_data.last_mut().expect("scalar arena reserved") as *mut u32
                                as *mut c_void;
                    }
                    E_PDU_PT::PDU_PT_SNUM32 => {
                        scalar_i32_data.push(*borrowed_param.snum32_data()?);
                        raw_param.pComParamData =
                            scalar_i32_data.last_mut().expect("scalar arena reserved") as *mut i32
                                as *mut c_void;
                    }
                    _ => {
                        let (owned_raw_param, storage) = if raw_param.ComParamDataType
                            == E_PDU_PT::PDU_PT_STRUCTFIELD
                        {
                            let struct_type =
                                borrowed_param.structfield_data()?.param_struct_type();
                            if Self::is_standard_struct_type(struct_type) {
                                OwnedParamItem::from_borrowed_parts(borrowed_param)?
                            } else {
                                let entry_size =
                                    vendor_entry_size_resolver(struct_type).ok_or(
                                    DPduApiError::Unsupported(
                                        "vendor specific structfield clone requires entry size for ComParamStructType",
                                    ),
                                )?;
                                OwnedParamItem::from_borrowed_parts_with_vendor_entry_size(
                                    borrowed_param,
                                    entry_size,
                                )?
                            }
                        } else {
                            OwnedParamItem::from_borrowed_parts(borrowed_param)?
                        };
                        raw_param = owned_raw_param;
                        param_storages.push(storage);
                    }
                }
                param_items.push(raw_param);
            }

            let entry_len = param_items.len() - start_index;
            let params_ptr = if entry_len == 0 {
                std::ptr::null_mut()
            } else {
                unsafe { param_items.as_mut_ptr().add(start_index) }
            };

            unique_data.push(PDU_ECU_UNIQUE_RESP_DATA {
                UniqueRespIdentifier: entry.UniqueRespIdentifier,
                NumParamItems: entry_len as UNUM32,
                pParams: params_ptr,
            });
        }

        let mut borrowed = BorrowedUniqueRespIdTableItem(item.0);
        borrowed.0.pUniqueData = vec_ptr_or_null(&mut unique_data);
        borrowed.0.NumEntries = unique_data.len() as UNUM32;
        Ok(Self {
            borrowed,
            unique_data,
            param_items,
            param_storages,
            scalar_u8_data,
            scalar_i8_data,
            scalar_u16_data,
            scalar_i16_data,
            scalar_u32_data,
            scalar_i32_data,
        })
    }

    fn from_borrowed(item: &BorrowedUniqueRespIdTableItem) -> Result<Self, DPduApiError> {
        Self::from_borrowed_with_vendor_entry_size_resolver(item, |_| None)
    }

    fn from_borrowed_with_vendor_entry_size(
        item: &BorrowedUniqueRespIdTableItem,
        vendor_entry_size: usize,
    ) -> Result<Self, DPduApiError> {
        if Self::has_multiple_vendor_struct_types(item)? {
            return Err(DPduApiError::Unsupported(
                "multiple vendor structfield types require per-type entry size resolver",
            ));
        }
        Self::from_borrowed_with_vendor_entry_size_resolver(item, |_| Some(vendor_entry_size))
    }

    #[inline(never)]
    fn keep_fields_alive(&self) {
        std::hint::black_box(self.unique_data.as_ptr());
        std::hint::black_box(self.unique_data.len());

        std::hint::black_box(self.param_items.as_ptr());
        std::hint::black_box(self.param_items.len());

        std::hint::black_box(self.param_storages.as_ptr());
        std::hint::black_box(self.param_storages.len());
        for storage in &self.param_storages {
            storage.keep_alive();
        }

        std::hint::black_box(self.scalar_u8_data.as_ptr());
        std::hint::black_box(self.scalar_u8_data.len());
        std::hint::black_box(self.scalar_i8_data.as_ptr());
        std::hint::black_box(self.scalar_i8_data.len());
        std::hint::black_box(self.scalar_u16_data.as_ptr());
        std::hint::black_box(self.scalar_u16_data.len());
        std::hint::black_box(self.scalar_i16_data.as_ptr());
        std::hint::black_box(self.scalar_i16_data.len());
        std::hint::black_box(self.scalar_u32_data.as_ptr());
        std::hint::black_box(self.scalar_u32_data.len());
        std::hint::black_box(self.scalar_i32_data.as_ptr());
        std::hint::black_box(self.scalar_i32_data.len());
    }
}

impl Borrow<BorrowedUniqueRespIdTableItem> for OwnedUniqueRespIdTableItem {
    fn borrow(&self) -> &BorrowedUniqueRespIdTableItem {
        self.keep_fields_alive();
        &self.borrowed
    }
}

impl ToOwned for BorrowedUniqueRespIdTableItem {
    type Owned = OwnedUniqueRespIdTableItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedUniqueRespIdTableItem::from_borrowed(self)
            .expect("unique response table is not cloneable")
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
    fn owned_unique_response_table_keeps_nested_param_pointer_after_move() {
        let mut value = Box::new(0x5566_u32);
        let mut params = vec![PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 9,
            ComParamDataType: iso22900_sys::bindings::E_PDU_PT::PDU_PT_UNUM32,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: (&mut *value as *mut u32).cast(),
        }];
        let mut entries = vec![PDU_ECU_UNIQUE_RESP_DATA {
            UniqueRespIdentifier: 8,
            NumParamItems: params.len() as UNUM32,
            pParams: params.as_mut_ptr(),
        }];

        let borrowed = BorrowedUniqueRespIdTableItem(PDU_UNIQUE_RESP_ID_TABLE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE,
            NumEntries: entries.len() as UNUM32,
            pUniqueData: entries.as_mut_ptr(),
        });

        let owned =
            OwnedUniqueRespIdTableItem::from_borrowed(&borrowed).expect("clone should succeed");
        let unique_ptr_before = owned.borrowed.0.pUniqueData;
        assert!(!unique_ptr_before.is_null());
        let params_ptr_before = unsafe { (*unique_ptr_before).pParams };
        assert!(!params_ptr_before.is_null());
        let data_ptr_before = unsafe { (*params_ptr_before).pComParamData.cast::<u32>() };
        assert!(!data_ptr_before.is_null());
        assert_eq!(unsafe { *data_ptr_before }, 0x5566);

        let owned = move_value(owned);
        let unique_ptr_after = owned.borrowed.0.pUniqueData;
        assert_eq!(unique_ptr_after, unique_ptr_before);
        let params_ptr_after = unsafe { (*unique_ptr_after).pParams };
        assert_eq!(params_ptr_after, params_ptr_before);
        let data_ptr_after = unsafe { (*params_ptr_after).pComParamData.cast::<u32>() };
        assert_eq!(data_ptr_after, data_ptr_before);
        assert_eq!(unsafe { *data_ptr_after }, 0x5566);
    }

    #[test]
    fn unique_response_vendor_structfield_requires_entry_size() {
        let mut vendor_payload = vec![0xAA_u8, 0xBB, 0xCC, 0xDD];
        let mut structfield = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0001),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload.as_mut_ptr().cast(),
        };
        let mut params = vec![PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 9,
            ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: (&mut structfield as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
        }];
        let mut entries = vec![PDU_ECU_UNIQUE_RESP_DATA {
            UniqueRespIdentifier: 8,
            NumParamItems: params.len() as UNUM32,
            pParams: params.as_mut_ptr(),
        }];

        let borrowed = BorrowedUniqueRespIdTableItem(PDU_UNIQUE_RESP_ID_TABLE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE,
            NumEntries: entries.len() as UNUM32,
            pUniqueData: entries.as_mut_ptr(),
        });

        let err = OwnedUniqueRespIdTableItem::from_borrowed(&borrowed)
            .expect_err("vendor structfield should require entry size");
        match err {
            DPduApiError::Unsupported(name) => {
                assert_eq!(
                    name,
                    "vendor specific structfield clone requires entry size for ComParamStructType"
                )
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn unique_response_vendor_structfield_preserved_with_entry_size() {
        let mut vendor_payload = vec![0x11_u8, 0x22, 0x33, 0x44];
        let mut structfield = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0001),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload.as_mut_ptr().cast(),
        };
        let mut params = vec![PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: 10,
            ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
            ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
            pComParamData: (&mut structfield as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
        }];
        let mut entries = vec![PDU_ECU_UNIQUE_RESP_DATA {
            UniqueRespIdentifier: 9,
            NumParamItems: params.len() as UNUM32,
            pParams: params.as_mut_ptr(),
        }];

        let borrowed = BorrowedUniqueRespIdTableItem(PDU_UNIQUE_RESP_ID_TABLE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE,
            NumEntries: entries.len() as UNUM32,
            pUniqueData: entries.as_mut_ptr(),
        });

        let owned = OwnedUniqueRespIdTableItem::from_borrowed_with_vendor_entry_size(&borrowed, 2)
            .expect("vendor structfield should clone with entry size");
        assert_eq!(owned.borrowed.0.NumEntries, 1);
        assert!(!owned.borrowed.0.pUniqueData.is_null());

        let first_entry = unsafe { &*owned.borrowed.0.pUniqueData };
        assert_eq!(first_entry.NumParamItems, 1);
        assert!(!first_entry.pParams.is_null());

        let first_param = unsafe { &*first_entry.pParams };
        let meta = unsafe {
            &*first_param
                .pComParamData
                .cast::<PDU_PARAM_STRUCTFIELD_DATA>()
        };
        assert_eq!(meta.ComParamStructType, T_PDU_CPST(0x8000_0001_u32 as _));
        assert_eq!(meta.ParamActEntries, 2);
        assert!(!meta.pStructArray.is_null());

        let raw_bytes = unsafe { std::slice::from_raw_parts(meta.pStructArray.cast::<u8>(), 4) };
        assert_eq!(raw_bytes, &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn unique_response_mixed_vendor_types_require_resolver() {
        let mut vendor_payload_a = vec![0xA1_u8, 0xA2, 0xA3, 0xA4];
        let mut vendor_payload_b = vec![0xB1_u8, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6];

        let mut structfield_a = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0101),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload_a.as_mut_ptr().cast(),
        };
        let mut structfield_b = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0202),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload_b.as_mut_ptr().cast(),
        };

        let mut params = vec![
            PDU_PARAM_ITEM {
                ItemType: E_PDU_IT::PDU_IT_PARAM,
                ComParamId: 10,
                ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
                ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
                pComParamData: (&mut structfield_a as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
            },
            PDU_PARAM_ITEM {
                ItemType: E_PDU_IT::PDU_IT_PARAM,
                ComParamId: 11,
                ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
                ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
                pComParamData: (&mut structfield_b as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
            },
        ];

        let mut entries = vec![PDU_ECU_UNIQUE_RESP_DATA {
            UniqueRespIdentifier: 9,
            NumParamItems: params.len() as UNUM32,
            pParams: params.as_mut_ptr(),
        }];

        let borrowed = BorrowedUniqueRespIdTableItem(PDU_UNIQUE_RESP_ID_TABLE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE,
            NumEntries: entries.len() as UNUM32,
            pUniqueData: entries.as_mut_ptr(),
        });

        // Safety: test-only fixed struct payloads, sized to match the
        // supplied entry size.
        let err = unsafe { borrowed.to_owned_with_vendor_entry_size(2) }
            .expect_err("mixed vendor struct types must require resolver");
        match err {
            DPduApiError::Unsupported(name) => {
                assert_eq!(
                    name,
                    "multiple vendor structfield types require per-type entry size resolver"
                )
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // The cast matters on *-windows-msvc, where the enum is c_int.
    #[allow(clippy::unnecessary_cast)]
    #[test]
    fn unique_response_mixed_vendor_types_preserved_with_resolver() {
        let mut vendor_payload_a = vec![0xC1_u8, 0xC2, 0xC3, 0xC4];
        let mut vendor_payload_b = vec![0xD1_u8, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6];

        let mut structfield_a = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0101),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload_a.as_mut_ptr().cast(),
        };
        let mut structfield_b = PDU_PARAM_STRUCTFIELD_DATA {
            ComParamStructType: vendor_struct_type(0x8000_0202),
            ParamMaxEntries: 2,
            ParamActEntries: 2,
            pStructArray: vendor_payload_b.as_mut_ptr().cast(),
        };

        let mut params = vec![
            PDU_PARAM_ITEM {
                ItemType: E_PDU_IT::PDU_IT_PARAM,
                ComParamId: 12,
                ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
                ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
                pComParamData: (&mut structfield_a as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
            },
            PDU_PARAM_ITEM {
                ItemType: E_PDU_IT::PDU_IT_PARAM,
                ComParamId: 13,
                ComParamDataType: E_PDU_PT::PDU_PT_STRUCTFIELD,
                ComParamClass: iso22900_sys::bindings::E_PDU_PC::PDU_PC_COM,
                pComParamData: (&mut structfield_b as *mut PDU_PARAM_STRUCTFIELD_DATA).cast(),
            },
        ];

        let mut entries = vec![PDU_ECU_UNIQUE_RESP_DATA {
            UniqueRespIdentifier: 10,
            NumParamItems: params.len() as UNUM32,
            pParams: params.as_mut_ptr(),
        }];

        let borrowed = BorrowedUniqueRespIdTableItem(PDU_UNIQUE_RESP_ID_TABLE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE,
            NumEntries: entries.len() as UNUM32,
            pUniqueData: entries.as_mut_ptr(),
        });

        // Safety: test-only fixed struct payloads, sized to match the
        // resolver's returned entry sizes.
        let owned = unsafe {
            borrowed.to_owned_with_vendor_entry_size_resolver(|struct_type| {
                match struct_type.0 as u32 {
                    0x8000_0101 => Some(2),
                    0x8000_0202 => Some(3),
                    _ => None,
                }
            })
        }
        .expect("resolver should preserve mixed vendor struct types");

        let first_entry = unsafe { &*owned.borrowed.0.pUniqueData };
        assert_eq!(first_entry.NumParamItems, 2);
        let params_slice = unsafe {
            std::slice::from_raw_parts(first_entry.pParams, first_entry.NumParamItems as usize)
        };

        let meta_a = unsafe {
            &*params_slice[0]
                .pComParamData
                .cast::<PDU_PARAM_STRUCTFIELD_DATA>()
        };
        let meta_b = unsafe {
            &*params_slice[1]
                .pComParamData
                .cast::<PDU_PARAM_STRUCTFIELD_DATA>()
        };

        let bytes_a = unsafe { std::slice::from_raw_parts(meta_a.pStructArray.cast::<u8>(), 4) };
        let bytes_b = unsafe { std::slice::from_raw_parts(meta_b.pStructArray.cast::<u8>(), 6) };
        assert_eq!(bytes_a, &[0xC1, 0xC2, 0xC3, 0xC4]);
        assert_eq!(bytes_b, &[0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6]);
    }
}
