include!("bindings/vci.service.rs");

mod rich_error;
pub mod token;
pub use rich_error::{error_detail_from_status, status_with_error_detail};
