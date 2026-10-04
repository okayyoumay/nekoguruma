use std::borrow::{Borrow, ToOwned};

use super::*;

impl BorrowedResourceStatusItem {
    pub(crate) fn entries_raw(&self) -> Result<&[PDU_RSC_STATUS_DATA], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pResourceStatusData,
                self.0.NumEntries,
                "resource status data",
            )
        }
    }

    /// Returns immutable resource-status entries.
    pub fn entries(&self) -> Result<&[BorrowedResourceStatusData], DPduApiError> {
        let raw = self.entries_raw()?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedResourceStatusData, raw.len())
        })
    }

    /// Returns mutable resource-status entries.
    pub fn entries_mut(&mut self) -> Result<&mut [BorrowedResourceStatusData], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pResourceStatusData as *mut BorrowedResourceStatusData,
                self.0.NumEntries,
                "resource status data array",
            )
        }
    }
}

impl OwnedResourceStatusItem {
    pub(crate) fn from_entries<C: IntoIterator<Item = (ModuleHandle, ResourceId)>>(
        entries: C,
    ) -> Self {
        let iter = entries.into_iter();
        let (lower_bound, upper_bound) = iter.size_hint();
        let reserve = upper_bound.unwrap_or(lower_bound);
        let mut resource_status_data = Vec::with_capacity(reserve);
        for (module_handle, resource_id) in iter {
            resource_status_data.push(PDU_RSC_STATUS_DATA {
                hMod: module_handle.0,
                ResourceId: resource_id.0,
                ResourceStatus: 0,
            });
        }

        Self::from_resource_status_data(resource_status_data)
    }

    pub(crate) fn as_mut_raw(&mut self) -> &mut PDU_RSC_STATUS_ITEM {
        self.borrowed.0.NumEntries = self.resource_status_data.len() as UNUM32;
        self.borrowed.0.pResourceStatusData = vec_ptr_or_null(&mut self.resource_status_data);

        &mut self.borrowed.0
    }

    fn from_resource_status_data(mut resource_status_data: Vec<PDU_RSC_STATUS_DATA>) -> Self {
        let borrowed = BorrowedResourceStatusItem(PDU_RSC_STATUS_ITEM {
            ItemType: iso22900_sys::bindings::E_PDU_IT::PDU_IT_RSC_STATUS,
            NumEntries: resource_status_data.len() as UNUM32,
            pResourceStatusData: vec_ptr_or_null(&mut resource_status_data),
        });

        Self {
            borrowed,
            resource_status_data,
        }
    }

    fn from_borrowed(item: &BorrowedResourceStatusItem) -> Result<Self, DPduApiError> {
        Ok(Self::from_resource_status_data(
            item.entries_raw()?.to_vec(),
        ))
    }
}

impl Borrow<BorrowedResourceStatusItem> for OwnedResourceStatusItem {
    fn borrow(&self) -> &BorrowedResourceStatusItem {
        &self.borrowed
    }
}

impl ToOwned for BorrowedResourceStatusItem {
    type Owned = OwnedResourceStatusItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedResourceStatusItem::from_borrowed(self).expect("resource status item is not cloneable")
    }
}

impl BorrowedResourceIdItem {
    pub(crate) fn modules_raw(&self) -> Result<&[PDU_RSC_ID_ITEM_DATA], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pResourceIdDataArray,
                self.0.NumModules,
                "resource id data",
            )
        }
    }

    /// Returns immutable module/resource-id groups.
    pub fn modules(&self) -> Result<&[BorrowedResourceIdData], DPduApiError> {
        let raw = self.modules_raw()?;
        Ok(unsafe {
            std::slice::from_raw_parts(raw.as_ptr() as *const BorrowedResourceIdData, raw.len())
        })
    }

    /// Returns mutable module/resource-id groups.
    pub fn modules_mut(&mut self) -> Result<&mut [BorrowedResourceIdData], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pResourceIdDataArray as *mut BorrowedResourceIdData,
                self.0.NumModules,
                "resource id data array",
            )
        }
    }
}

impl OwnedResourceIdItem {
    fn from_borrowed(item: &BorrowedResourceIdItem) -> Result<Self, DPduApiError> {
        let modules = item.modules_raw()?;
        let mut module_data = Vec::with_capacity(modules.len());
        let total_ids = modules
            .iter()
            .map(|module| module.NumIds as usize)
            .sum::<usize>();
        let mut resource_id_data = Vec::with_capacity(total_ids);

        for module in modules {
            let ids =
                unsafe { ffi_slice(module.pResourceIdArray, module.NumIds, "resource id array") }?;
            let start = resource_id_data.len();
            resource_id_data.extend_from_slice(ids);
            module_data.push(PDU_RSC_ID_ITEM_DATA {
                hMod: module.hMod,
                NumIds: ids.len() as UNUM32,
                pResourceIdArray: if ids.is_empty() {
                    std::ptr::null_mut()
                } else {
                    unsafe { resource_id_data.as_mut_ptr().add(start) }
                },
            });
        }

        let mut borrowed = BorrowedResourceIdItem(item.0);
        borrowed.0.pResourceIdDataArray = vec_ptr_or_null(&mut module_data);
        borrowed.0.NumModules = module_data.len() as UNUM32;
        Ok(Self {
            borrowed,
            module_data,
            resource_id_data,
        })
    }

    #[inline(never)]
    fn keep_fields_alive(&self) {
        std::hint::black_box(self.module_data.as_ptr());
        std::hint::black_box(self.module_data.len());

        std::hint::black_box(self.resource_id_data.as_ptr());
        std::hint::black_box(self.resource_id_data.len());
    }
}

impl Borrow<BorrowedResourceIdItem> for OwnedResourceIdItem {
    fn borrow(&self) -> &BorrowedResourceIdItem {
        self.keep_fields_alive();
        &self.borrowed
    }
}

impl ToOwned for BorrowedResourceIdItem {
    type Owned = OwnedResourceIdItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedResourceIdItem::from_borrowed(self).expect("resource id item is not cloneable")
    }
}

impl BorrowedResourceConflictItem {
    pub(crate) fn entries_raw(&self) -> Result<&[PDU_RSC_CONFLICT_DATA], DPduApiError> {
        unsafe {
            ffi_slice(
                self.0.pRscConflictData,
                self.0.NumEntries,
                "conflicting resource data",
            )
        }
    }

    /// Returns immutable conflicting-resource entries.
    pub fn entries(&self) -> Result<&[BorrowedResourceConflictData], DPduApiError> {
        let raw = self.entries_raw()?;
        Ok(unsafe {
            std::slice::from_raw_parts(
                raw.as_ptr() as *const BorrowedResourceConflictData,
                raw.len(),
            )
        })
    }

    /// Returns mutable conflicting-resource entries.
    pub fn entries_mut(&mut self) -> Result<&mut [BorrowedResourceConflictData], DPduApiError> {
        unsafe {
            ffi_slice_mut(
                self.0.pRscConflictData as *mut BorrowedResourceConflictData,
                self.0.NumEntries,
                "conflicting resource data array",
            )
        }
    }
}

impl OwnedResourceConflictItem {
    fn from_borrowed(item: &BorrowedResourceConflictItem) -> Result<Self, DPduApiError> {
        let mut conflict_data = item.entries_raw()?.to_vec();
        let mut borrowed = BorrowedResourceConflictItem(item.0);
        borrowed.0.pRscConflictData = vec_ptr_or_null(&mut conflict_data);
        borrowed.0.NumEntries = conflict_data.len() as UNUM32;
        Ok(Self {
            borrowed,
            conflict_data,
        })
    }

    #[inline(never)]
    fn keep_fields_alive(&self) {
        std::hint::black_box(self.conflict_data.as_ptr());
        std::hint::black_box(self.conflict_data.len());
    }
}

impl Borrow<BorrowedResourceConflictItem> for OwnedResourceConflictItem {
    fn borrow(&self) -> &BorrowedResourceConflictItem {
        self.keep_fields_alive();
        &self.borrowed
    }
}

impl ToOwned for BorrowedResourceConflictItem {
    type Owned = OwnedResourceConflictItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedResourceConflictItem::from_borrowed(self).expect("conflict item is not cloneable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_value<T>(value: T) -> T {
        value
    }

    #[test]
    fn owned_resource_status_item_keeps_data_pointer_after_move() {
        let mut owned = OwnedResourceStatusItem::from_entries([(ModuleHandle(1), ResourceId(2))]);
        let ptr_before = owned.as_mut_raw().pResourceStatusData;
        assert!(!ptr_before.is_null());
        assert_eq!(unsafe { (*ptr_before).ResourceId }, 2);

        let mut owned = move_value(owned);
        let ptr_after = owned.as_mut_raw().pResourceStatusData;
        assert_eq!(ptr_after, ptr_before);
        assert_eq!(unsafe { (*ptr_after).hMod }, 1);
    }

    #[test]
    fn owned_resource_id_item_keeps_nested_pointer_after_move() {
        let mut ids = vec![10_u32, 20_u32];
        let mut module_data = vec![PDU_RSC_ID_ITEM_DATA {
            hMod: 7,
            NumIds: ids.len() as UNUM32,
            pResourceIdArray: ids.as_mut_ptr(),
        }];
        let borrowed = BorrowedResourceIdItem(PDU_RSC_ID_ITEM {
            ItemType: E_PDU_IT::PDU_IT_RSC_ID,
            NumModules: module_data.len() as UNUM32,
            pResourceIdDataArray: module_data.as_mut_ptr(),
        });

        let owned = OwnedResourceIdItem::from_borrowed(&borrowed).expect("clone should succeed");
        let ptr_before = owned.borrowed.0.pResourceIdDataArray;
        assert!(!ptr_before.is_null());
        let nested_before = unsafe { (*ptr_before).pResourceIdArray };
        assert!(!nested_before.is_null());
        assert_eq!(unsafe { *nested_before }, 10);

        let owned = move_value(owned);
        let ptr_after = owned.borrowed.0.pResourceIdDataArray;
        assert_eq!(ptr_after, ptr_before);
        let nested_after = unsafe { (*ptr_after).pResourceIdArray };
        assert_eq!(nested_after, nested_before);
        assert_eq!(unsafe { *nested_after.add(1) }, 20);
    }

    #[test]
    fn owned_resource_conflict_item_keeps_data_pointer_after_move() {
        let mut entries = vec![PDU_RSC_CONFLICT_DATA {
            hMod: 3,
            ResourceId: 4,
        }];
        let borrowed = BorrowedResourceConflictItem(PDU_RSC_CONFLICT_ITEM {
            ItemType: E_PDU_IT::PDU_IT_RSC_CONFLICT,
            NumEntries: entries.len() as UNUM32,
            pRscConflictData: entries.as_mut_ptr(),
        });

        let owned =
            OwnedResourceConflictItem::from_borrowed(&borrowed).expect("clone should succeed");
        let ptr_before = owned.borrowed.0.pRscConflictData;
        assert!(!ptr_before.is_null());
        assert_eq!(unsafe { (*ptr_before).ResourceId }, 4);

        let owned = move_value(owned);
        let ptr_after = owned.borrowed.0.pRscConflictData;
        assert_eq!(ptr_after, ptr_before);
        assert_eq!(unsafe { (*ptr_after).hMod }, 3);
    }
}
