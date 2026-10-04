use std::borrow::{Borrow, ToOwned};

use super::*;

impl BorrowedEventItem {
    /// Returns the raw event item type.
    pub fn item_type(&self) -> T_PDU_IT {
        self.0.ItemType
    }

    /// Returns the communication primitive handle.
    pub fn com_primitive_handle(&self) -> UNUM32 {
        self.0.hCop
    }

    /// Returns the opaque primitive tag value as usize.
    pub fn cop_tag(&self) -> usize {
        self.0.pCoPTag as usize
    }

    /// Returns the event timestamp.
    pub fn timestamp(&self) -> UNUM32 {
        self.0.Timestamp
    }

    /// Interprets event payload pointer as T.
    pub fn data<T>(&self) -> Result<&T, DPduApiError> {
        unsafe { (self.0.pData as *const T).as_ref() }
            .ok_or(DPduApiError::NullPointer("event item data"))
    }

    /// Interprets mutable event payload pointer as T.
    pub fn data_mut<T>(&mut self) -> Result<&mut T, DPduApiError> {
        unsafe { (self.0.pData as *mut T).as_mut() }
            .ok_or(DPduApiError::NullPointer("event item data"))
    }

    fn ensure_item_type(
        &self,
        expected: T_PDU_IT,
        payload_name: &'static str,
    ) -> Result<(), DPduApiError> {
        if self.item_type() == expected {
            Ok(())
        } else {
            Err(DPduApiError::Unsupported(payload_name))
        }
    }

    fn typed_data_ref<'a, T: 'a, U>(
        &'a self,
        expected: T_PDU_IT,
        payload_name: &'static str,
    ) -> Result<&'a U, DPduApiError> {
        self.ensure_item_type(expected, payload_name)?;
        let data = self.data::<T>()?;
        Ok(transparent_ref::<T, U>(data))
    }

    fn typed_data_mut<'a, T: 'a, U>(
        &'a mut self,
        expected: T_PDU_IT,
        payload_name: &'static str,
    ) -> Result<&'a mut U, DPduApiError> {
        self.ensure_item_type(expected, payload_name)?;
        let data = self.data_mut::<T>()?;
        Ok(transparent_mut::<T, U>(data))
    }

    /// Returns result payload when item type is result.
    pub fn result_data(&self) -> Result<&BorrowedResultData, DPduApiError> {
        self.typed_data_ref::<PDU_RESULT_DATA, BorrowedResultData>(
            E_PDU_IT::PDU_IT_RESULT,
            "event item result payload",
        )
    }

    /// Returns mutable result payload when item type is result.
    pub fn result_data_mut(&mut self) -> Result<&mut BorrowedResultData, DPduApiError> {
        self.typed_data_mut::<PDU_RESULT_DATA, BorrowedResultData>(
            E_PDU_IT::PDU_IT_RESULT,
            "event item result payload",
        )
    }

    /// Returns status payload when item type is status.
    pub fn status_data(&self) -> Result<&BorrowedStatusData, DPduApiError> {
        self.typed_data_ref::<PDU_STATUS_DATA, BorrowedStatusData>(
            E_PDU_IT::PDU_IT_STATUS,
            "event item status payload",
        )
    }

    /// Returns mutable status payload when item type is status.
    pub fn status_data_mut(&mut self) -> Result<&mut BorrowedStatusData, DPduApiError> {
        self.typed_data_mut::<PDU_STATUS_DATA, BorrowedStatusData>(
            E_PDU_IT::PDU_IT_STATUS,
            "event item status payload",
        )
    }

    /// Returns error payload when item type is error.
    pub fn error_data(&self) -> Result<&BorrowedErrorData, DPduApiError> {
        self.typed_data_ref::<PDU_ERROR_DATA, BorrowedErrorData>(
            E_PDU_IT::PDU_IT_ERROR,
            "event item error payload",
        )
    }

    /// Returns mutable error payload when item type is error.
    pub fn error_data_mut(&mut self) -> Result<&mut BorrowedErrorData, DPduApiError> {
        self.typed_data_mut::<PDU_ERROR_DATA, BorrowedErrorData>(
            E_PDU_IT::PDU_IT_ERROR,
            "event item error payload",
        )
    }

    /// Returns info payload when item type is info.
    pub fn info_data(&self) -> Result<&BorrowedInfoData, DPduApiError> {
        self.typed_data_ref::<PDU_INFO_DATA, BorrowedInfoData>(
            E_PDU_IT::PDU_IT_INFO,
            "event item info payload",
        )
    }

    /// Returns mutable info payload when item type is info.
    pub fn info_data_mut(&mut self) -> Result<&mut BorrowedInfoData, DPduApiError> {
        self.typed_data_mut::<PDU_INFO_DATA, BorrowedInfoData>(
            E_PDU_IT::PDU_IT_INFO,
            "event item info payload",
        )
    }

    /// Dispatches immutable payload handling to a visitor.
    pub fn visit<V: EventItemVisitor>(&self, visitor: &mut V) -> Result<V::Output, DPduApiError> {
        match self.item_type() {
            E_PDU_IT::PDU_IT_RESULT => visitor.visit_result(self.result_data()?),
            E_PDU_IT::PDU_IT_STATUS => visitor.visit_status(self.status_data()?),
            E_PDU_IT::PDU_IT_ERROR => visitor.visit_error(self.error_data()?),
            E_PDU_IT::PDU_IT_INFO => visitor.visit_info(self.info_data()?),
            _ => Err(DPduApiError::Unsupported("event item type")),
        }
    }

    /// Dispatches mutable payload handling to a visitor.
    pub fn visit_mut<V: EventItemVisitorMut>(
        &mut self,
        visitor: &mut V,
    ) -> Result<V::Output, DPduApiError> {
        match self.item_type() {
            E_PDU_IT::PDU_IT_RESULT => visitor.visit_result(self.result_data_mut()?),
            E_PDU_IT::PDU_IT_STATUS => visitor.visit_status(self.status_data_mut()?),
            E_PDU_IT::PDU_IT_ERROR => visitor.visit_error(self.error_data_mut()?),
            E_PDU_IT::PDU_IT_INFO => visitor.visit_info(self.info_data_mut()?),
            _ => Err(DPduApiError::Unsupported("event item type")),
        }
    }
}

impl Borrow<BorrowedEventItem> for OwnedEventItem {
    fn borrow(&self) -> &BorrowedEventItem {
        &self.borrowed
    }
}

impl ToOwned for BorrowedEventItem {
    type Owned = OwnedEventItem;

    fn to_owned(&self) -> Self::Owned {
        OwnedEventItem {
            borrowed: BorrowedEventItem(self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_data_rejects_wrong_item_type() {
        let raw = PDU_EVENT_ITEM {
            ItemType: E_PDU_IT::PDU_IT_STATUS,
            hCop: 0,
            pCoPTag: std::ptr::null_mut(),
            Timestamp: 0,
            pData: std::ptr::null_mut(),
        };
        let item = BorrowedEventItem(raw);

        let err = item.result_data().expect_err("expected type mismatch");
        match err {
            DPduApiError::Unsupported(name) => assert_eq!(name, "event item result payload"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn result_data_detects_null_payload() {
        let raw = PDU_EVENT_ITEM {
            ItemType: E_PDU_IT::PDU_IT_RESULT,
            hCop: 0,
            pCoPTag: std::ptr::null_mut(),
            Timestamp: 0,
            pData: std::ptr::null_mut(),
        };
        let item = BorrowedEventItem(raw);

        let err = item.result_data().expect_err("expected null payload error");
        match err {
            DPduApiError::NullPointer(name) => assert_eq!(name, "event item data"),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
