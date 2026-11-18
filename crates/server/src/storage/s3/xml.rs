//! The XML the object store reads that rusty-s3 has no action for (ADR-492): the answer to
//! ListMultipartUploads, and the error code of an error body. Everything here is bounded by the
//! caller's body limits, and nothing is formatted into an error.

use instant_xml::FromXml;

/// The XML namespace of S3's answers.
const S3_NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

/// The answer to ListMultipartUploads (one page).
#[derive(Debug, FromXml)]
#[xml(rename = "ListMultipartUploadsResult", ns(S3_NS))]
pub(super) struct UploadsPage {
    #[xml(rename = "IsTruncated")]
    pub(super) is_truncated: bool,
    #[xml(rename = "NextKeyMarker")]
    pub(super) next_key_marker: Option<String>,
    #[xml(rename = "NextUploadIdMarker")]
    pub(super) next_upload_id_marker: Option<String>,
    #[xml(rename = "Upload")]
    pub(super) uploads: Vec<UploadEntry>,
}

/// One upload of the page. `Debug` is not derived: the fields are a key and an upload ID.
#[derive(FromXml)]
#[xml(rename = "Upload", ns(S3_NS))]
pub(super) struct UploadEntry {
    #[xml(rename = "Key")]
    pub(super) key: String,
    #[xml(rename = "UploadId")]
    pub(super) upload_id: String,
    #[xml(rename = "Initiated")]
    pub(super) initiated: String,
}

impl std::fmt::Debug for UploadEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UploadEntry(..)")
    }
}

/// Parses one page of ListMultipartUploads.
pub(super) fn parse_uploads(text: &str) -> Option<UploadsPage> {
    instant_xml::from_str(text).ok()
}

/// The codes of an error body that the store acts on; any other is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorCode {
    NoSuchUpload,
    InvalidPart,
    BucketAlreadyOwnedByYou,
    Other,
}

/// The code of an S3 error body (the text of its `Code` element), `Other` if the body has none or
/// is not UTF-8. A body cut at the error limit still yields its code when the code came first.
pub(super) fn error_code(body: &[u8]) -> ErrorCode {
    let Ok(text) = std::str::from_utf8(body) else {
        return ErrorCode::Other;
    };
    let Some(start) = text.find("<Code>") else {
        return ErrorCode::Other;
    };
    let rest = &text[start + "<Code>".len()..];
    let Some(end) = rest.find("</Code>") else {
        return ErrorCode::Other;
    };
    match &rest[..end] {
        "NoSuchUpload" => ErrorCode::NoSuchUpload,
        "InvalidPart" | "InvalidPartOrder" => ErrorCode::InvalidPart,
        "BucketAlreadyOwnedByYou" => ErrorCode::BucketAlreadyOwnedByYou,
        _ => ErrorCode::Other,
    }
}
