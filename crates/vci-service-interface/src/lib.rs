include!("bindings/vci.service.rs");

mod rich_error;
pub use rich_error::{error_detail_from_status, status_with_error_detail};
