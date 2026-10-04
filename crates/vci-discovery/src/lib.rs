//! Discovery of installed vendor libraries (7.1), used by the agent.
//!
//! - [`j2534`]: J2534 v04.04 devices from `HKLM\SOFTWARE\PassThruSupport.04.04` (Windows)
//! - [`pdu_api`]: D-PDU API implementations from the root description file `pdu_api_root.xml`
//!
//! Discovery only reports where libraries are and what they declare about themselves.
//! Pre-load verification (7.2) and ABI detection (7.3) are separate steps.
//! The Linux J2534 registration definition (7.1.1) is not implemented yet.
//!
//! Derived from the `j2534-0404-registry` and `iso22900-registry` crates.

pub mod j2534;
pub mod pdu_api;

/// Registry view. Always specified explicitly (`KEY_WOW64_32KEY` / `KEY_WOW64_64KEY`, 7.1),
/// so that a 64-bit agent also finds 32-bit installations and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistryView {
    Wow64_32,
    Wow64_64,
}

impl RegistryView {
    pub const ALL: [RegistryView; 2] = [RegistryView::Wow64_32, RegistryView::Wow64_64];

    #[cfg(windows)]
    fn flag(self) -> u32 {
        match self {
            RegistryView::Wow64_32 => winreg::enums::KEY_WOW64_32KEY,
            RegistryView::Wow64_64 => winreg::enums::KEY_WOW64_64KEY,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("registry discovery is only available on Windows")]
    RegistryUnsupported,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid XML in {path}: {source}")]
    Xml {
        path: String,
        #[source]
        source: roxmltree::Error,
    },
    #[error("invalid file URI {0:?}")]
    InvalidUri(String),
}
