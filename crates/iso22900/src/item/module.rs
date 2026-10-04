use super::*;

/// Borrowed wrapper over one `PDU_MODULE_DATA` entry.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedModuleData(pub PDU_MODULE_DATA);

impl BorrowedModuleData {
    /// Wraps a raw module data value.
    pub fn from_raw(raw: PDU_MODULE_DATA) -> Self {
        Self(raw)
    }

    /// Returns module type ID.
    pub fn module_type_id(&self) -> UNUM32 {
        self.0.ModuleTypeId
    }

    /// Returns module handle.
    pub fn module_handle(&self) -> UNUM32 {
        self.0.hMod
    }

    /// Returns vendor module name if present and valid UTF-8.
    pub fn vendor_module_name(&self) -> Option<&str> {
        if self.0.pVendorModuleName.is_null() {
            None
        } else {
            unsafe {
                std::ffi::CStr::from_ptr(self.0.pVendorModuleName)
                    .to_str()
                    .ok()
            }
        }
    }

    /// Returns additional vendor info if present and valid UTF-8.
    pub fn vendor_additional_info(&self) -> Option<&str> {
        if self.0.pVendorAdditionalInfo.is_null() {
            None
        } else {
            unsafe {
                std::ffi::CStr::from_ptr(self.0.pVendorAdditionalInfo)
                    .to_str()
                    .ok()
            }
        }
    }

    /// Returns module status code.
    pub fn module_status(&self) -> T_PDU_STATUS {
        self.0.ModuleStatus
    }
}

/// Borrowed module-item view returned by `PDUGetModuleIds`.
#[repr(transparent)]
#[derive(Debug)]
pub struct BorrowedModuleItem(pub PDU_MODULE_ITEM);

impl BorrowedModuleItem {
    /// Returns raw item type code.
    pub fn item_type(&self) -> T_PDU_IT {
        self.0.ItemType
    }

    /// Returns slice of raw module data for internal iteration
    pub(crate) fn entries_raw(&self) -> Result<&[PDU_MODULE_DATA], DPduApiError> {
        unsafe { ffi_slice(self.0.pModuleData, self.0.NumEntries, "module data") }
    }

    /// Returns slice of borrowed module data with safe accessors
    pub fn entries(&self) -> Result<&[BorrowedModuleData], DPduApiError> {
        let raw = self.entries_raw()?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedModuleData, raw.len())
        })
    }
}

/// Owned module item with stable backing strings/pointers.
#[derive(Debug)]
pub struct OwnedModuleItem {
    borrowed: BorrowedModuleItem,
    module_data: Vec<PDU_MODULE_DATA>,
    vendor_strings: Vec<(Option<CString>, Option<CString>)>,
}

impl OwnedModuleItem {
    fn from_borrowed(item: &BorrowedModuleItem) -> Result<Self, DPduApiError> {
        let entries = item.entries_raw()?;
        let mut module_data = Vec::with_capacity(entries.len());
        let mut vendor_strings = Vec::with_capacity(entries.len());

        for entry in entries {
            let vendor = (
                clone_c_string(entry.pVendorModuleName),
                clone_c_string(entry.pVendorAdditionalInfo),
            );
            let vendor_module_name = vendor.0.as_ref();
            let vendor_additional_info = vendor.1.as_ref();

            module_data.push(PDU_MODULE_DATA {
                ModuleTypeId: entry.ModuleTypeId,
                hMod: entry.hMod,
                pVendorModuleName: c_string_ptr(vendor_module_name),
                pVendorAdditionalInfo: c_string_ptr(vendor_additional_info),
                ModuleStatus: entry.ModuleStatus,
            });
            vendor_strings.push(vendor);
        }

        let mut owned = Self {
            borrowed: BorrowedModuleItem(item.0),
            module_data,
            vendor_strings,
        };
        owned.borrowed.0.pModuleData = vec_ptr_or_null(&mut owned.module_data);
        owned.borrowed.0.NumEntries = owned.module_data.len() as UNUM32;
        Ok(owned)
    }

    #[inline(never)]
    fn keep_vendor_fields_alive(&self) {
        std::hint::black_box(self.vendor_strings.as_ptr());
        std::hint::black_box(self.vendor_strings.len());
    }
}

impl Borrow<BorrowedModuleItem> for OwnedModuleItem {
    fn borrow(&self) -> &BorrowedModuleItem {
        self.keep_vendor_fields_alive();
        &self.borrowed
    }
}

impl ToOwned for BorrowedModuleItem {
    type Owned = OwnedModuleItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedModuleItem::from_borrowed(self).expect("module item is not cloneable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_value<T>(value: T) -> T {
        value
    }

    #[test]
    fn owned_module_item_keeps_string_pointers_after_move() {
        let vendor_name = CString::new("mod-a").expect("valid C string");
        let vendor_info = CString::new("info-a").expect("valid C string");
        let mut entries = vec![PDU_MODULE_DATA {
            ModuleTypeId: 1,
            hMod: 2,
            pVendorModuleName: vendor_name.as_ptr().cast_mut(),
            pVendorAdditionalInfo: vendor_info.as_ptr().cast_mut(),
            ModuleStatus: iso22900_sys::bindings::E_PDU_STATUS::PDU_MODST_AVAIL,
        }];
        let borrowed = BorrowedModuleItem(PDU_MODULE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_MODULE_ID,
            NumEntries: entries.len() as UNUM32,
            pModuleData: entries.as_mut_ptr(),
        });

        let owned = OwnedModuleItem::from_borrowed(&borrowed).expect("clone should succeed");
        let entries_ptr_before = owned.borrowed.0.pModuleData;
        assert!(!entries_ptr_before.is_null());
        let name_ptr_before = unsafe { (*entries_ptr_before).pVendorModuleName };
        let info_ptr_before = unsafe { (*entries_ptr_before).pVendorAdditionalInfo };
        assert!(!name_ptr_before.is_null());
        assert!(!info_ptr_before.is_null());
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(name_ptr_before).to_str().unwrap() },
            "mod-a"
        );

        let owned = move_value(owned);
        let entries_ptr_after = owned.borrowed.0.pModuleData;
        assert_eq!(entries_ptr_after, entries_ptr_before);
        let name_ptr_after = unsafe { (*entries_ptr_after).pVendorModuleName };
        let info_ptr_after = unsafe { (*entries_ptr_after).pVendorAdditionalInfo };
        assert_eq!(name_ptr_after, name_ptr_before);
        assert_eq!(info_ptr_after, info_ptr_before);
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(info_ptr_after).to_str().unwrap() },
            "info-a"
        );
    }
}
