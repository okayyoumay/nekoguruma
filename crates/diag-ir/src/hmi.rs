//! HMI forms and record templates as the VM sends them to the host, and the answers it gets
//! back (ADR-340 items 1 to 3).
//!
//! A form is a postcard-encoded [`HmiForm`]: the form or record template id and its parameters,
//! in the order the procedure gives them. The constant pool holds an [`HmiTemplate`], whose
//! parameters are constants or slots that `Op::HmiRequestFromStack` fills from the stack. Both
//! share the bounds below, which [`decode_form`] and [`decode_template`] enforce. An answer is a
//! status byte ([`ANSWERED`], [`DECLINED`] or [`TIMED_OUT`]) followed by the answer's UTF-8 text.

use serde::{Deserialize, Serialize};

/// The longest form or record template id, in bytes of UTF-8.
pub const MAX_HMI_ID_LEN: usize = 128;
/// The most parameters one form takes.
pub const MAX_HMI_PARAMS: usize = 32;
/// The longest parameter name, in bytes.
pub const MAX_HMI_KEY_LEN: usize = 64;
/// The longest encoded form or template, in bytes.
pub const MAX_HMI_FORM_LEN: usize = 16 << 10;
/// The longest answer the host returns, status byte included.
pub const MAX_HMI_ANSWER_LEN: usize = 16 << 10;

/// The answer's first byte when the operator answered.
pub const ANSWERED: u8 = 1;
/// The answer's first byte when the operator declined the form or record.
pub const DECLINED: u8 = 0;
/// The answer's first byte when the HMI timeout (design 5.6) expired first.
pub const TIMED_OUT: u8 = 2;

/// A form or record template request as the host receives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HmiForm {
    /// The form id, or the record template's `id`.
    pub id: String,
    /// The parameters in source order. A record template has none in this version.
    pub params: Vec<(String, HmiValue)>,
}

/// A parameter's value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HmiValue {
    Int(i64),
    Bool(bool),
    Text(String),
}

/// A form as the constant pool holds it: each parameter is a constant or a slot filled at run
/// time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HmiTemplate {
    pub id: String,
    pub params: Vec<(String, HmiParam)>,
}

/// A template parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HmiParam {
    Const(HmiValue),
    /// Filled by `Op::HmiRequestFromStack` from the stack, the first slot being the deepest.
    Runtime,
}

/// Why a form or template is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HmiError {
    #[error("the encoding is {0} bytes, more than {MAX_HMI_FORM_LEN}")]
    TooLong(usize),
    #[error("the bytes are not one encoded form")]
    Malformed,
    #[error("the id is not 1 to {MAX_HMI_ID_LEN} bytes")]
    BadId,
    #[error("more than {MAX_HMI_PARAMS} parameters")]
    TooManyParams,
    #[error("parameter name {0:?} is not an identifier of 1 to {MAX_HMI_KEY_LEN} bytes")]
    BadKey(String),
    #[error("parameter name {0:?} is given twice")]
    DuplicateKey(String),
}

/// Checks the id and the parameter names against the bounds of item 1.
fn check<'k>(id: &str, keys: impl ExactSizeIterator<Item = &'k str>) -> Result<(), HmiError> {
    if !(1..=MAX_HMI_ID_LEN).contains(&id.len()) {
        return Err(HmiError::BadId);
    }
    if keys.len() > MAX_HMI_PARAMS {
        return Err(HmiError::TooManyParams);
    }
    let mut seen: Vec<&str> = Vec::new();
    for key in keys {
        if !is_identifier(key) {
            return Err(HmiError::BadKey(key.to_owned()));
        }
        if seen.contains(&key) {
            return Err(HmiError::DuplicateKey(key.to_owned()));
        }
        seen.push(key);
    }
    Ok(())
}

/// `[A-Za-z_][A-Za-z0-9_]*`, 1 to [`MAX_HMI_KEY_LEN`] bytes.
fn is_identifier(key: &str) -> bool {
    let mut bytes = key.bytes();
    key.len() <= MAX_HMI_KEY_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Encodes `value` after `check`, refusing an encoding past [`MAX_HMI_FORM_LEN`].
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, HmiError> {
    let bytes = postcard::to_allocvec(value).map_err(|_| HmiError::Malformed)?;
    if bytes.len() > MAX_HMI_FORM_LEN {
        return Err(HmiError::TooLong(bytes.len()));
    }
    Ok(bytes)
}

/// Decodes one value that takes all of `bytes`, which are at most [`MAX_HMI_FORM_LEN`].
fn decode<'d, T: Deserialize<'d>>(bytes: &'d [u8]) -> Result<T, HmiError> {
    if bytes.len() > MAX_HMI_FORM_LEN {
        return Err(HmiError::TooLong(bytes.len()));
    }
    match postcard::take_from_bytes(bytes) {
        Ok((value, [])) => Ok(value),
        _ => Err(HmiError::Malformed),
    }
}

/// Encodes a form, checking every bound.
pub fn encode_form(form: &HmiForm) -> Result<Vec<u8>, HmiError> {
    check(&form.id, form.params.iter().map(|(key, _)| key.as_str()))?;
    encode(form)
}

/// Decodes a form, checking every bound.
pub fn decode_form(bytes: &[u8]) -> Result<HmiForm, HmiError> {
    let form: HmiForm = decode(bytes)?;
    check(&form.id, form.params.iter().map(|(key, _)| key.as_str()))?;
    Ok(form)
}

/// Encodes a template, checking every bound.
pub fn encode_template(template: &HmiTemplate) -> Result<Vec<u8>, HmiError> {
    check(
        &template.id,
        template.params.iter().map(|(key, _)| key.as_str()),
    )?;
    encode(template)
}

/// Decodes a template, checking every bound.
pub fn decode_template(bytes: &[u8]) -> Result<HmiTemplate, HmiError> {
    let template: HmiTemplate = decode(bytes)?;
    check(
        &template.id,
        template.params.iter().map(|(key, _)| key.as_str()),
    )?;
    Ok(template)
}

impl HmiTemplate {
    /// The number of [`HmiParam::Runtime`] slots.
    pub fn runtime_slots(&self) -> usize {
        self.params
            .iter()
            .filter(|(_, param)| *param == HmiParam::Runtime)
            .count()
    }

    /// The form with each run-time slot filled, in order, from `values`. `None` when `values`
    /// does not hold exactly one value per slot.
    pub fn fill(&self, values: impl IntoIterator<Item = HmiValue>) -> Option<HmiForm> {
        let mut values = values.into_iter();
        let params = self
            .params
            .iter()
            .map(|(key, param)| {
                let value = match param {
                    HmiParam::Const(value) => value.clone(),
                    HmiParam::Runtime => values.next()?,
                };
                Some((key.clone(), value))
            })
            .collect::<Option<_>>()?;
        values.next().is_none().then(|| HmiForm {
            id: self.id.clone(),
            params,
        })
    }
}

/// An answer of the operator: [`ANSWERED`] followed by `text`.
pub fn answered(text: &str) -> Vec<u8> {
    [&[ANSWERED][..], text.as_bytes()].concat()
}

/// The answer when the operator declined.
pub fn declined() -> Vec<u8> {
    vec![DECLINED]
}

/// The answer when the HMI timeout expired first.
pub fn timed_out() -> Vec<u8> {
    vec![TIMED_OUT]
}

#[cfg(test)]
#[path = "hmi_tests.rs"]
mod tests;
