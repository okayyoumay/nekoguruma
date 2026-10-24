include!("bindings/vci.service.rs");

pub mod mode;
mod rich_error;
pub mod token;
pub use rich_error::{
    error_detail_from_status, protocol_cannot_carry, start_refusal, status_with_error_detail,
};
