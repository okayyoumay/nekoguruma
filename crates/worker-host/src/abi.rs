//! ABI detection from the library header (7.3).
//!
//! The header decides which worker build can load the library: a process can only load a
//! library of its own architecture and bitness. [`Abi`] holds the ABI interpretation table
//! (7.1.2) as data; the calling convention and structure packing are applied by each worker
//! service's sys layer.

use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Abi {
    WinX86,
    WinX64,
    LinuxX86,
    LinuxX86_64,
    LinuxArm64,
    LinuxArmhf,
}

impl Abi {
    pub const ALL: [Abi; 6] = [
        Abi::WinX86,
        Abi::WinX64,
        Abi::LinuxX86,
        Abi::LinuxX86_64,
        Abi::LinuxArm64,
        Abi::LinuxArmhf,
    ];

    /// Name used in the ABI table (7.1.2) and as the worker directory name.
    pub fn name(self) -> &'static str {
        match self {
            Abi::WinX86 => "win-x86",
            Abi::WinX64 => "win-x64",
            Abi::LinuxX86 => "linux-x86",
            Abi::LinuxX86_64 => "linux-x86_64",
            Abi::LinuxArm64 => "linux-arm64",
            Abi::LinuxArmhf => "linux-armhf",
        }
    }

    /// Width of J2534 `unsigned long` in bytes inferred for this ABI (7.1.2 table). Used when
    /// the registration definition has no `long_size`.
    pub fn default_long_size(self) -> u8 {
        match self {
            Abi::LinuxX86_64 | Abi::LinuxArm64 => 8,
            Abi::WinX86 | Abi::WinX64 | Abi::LinuxX86 | Abi::LinuxArmhf => 4,
        }
    }

    /// Where this ABI's interpretation comes from (7.1.2), reported in `capabilities`.
    pub fn interpretation(self) -> Interpretation {
        match self {
            Abi::WinX86 | Abi::WinX64 => Interpretation::Standard,
            Abi::LinuxX86 | Abi::LinuxX86_64 => Interpretation::ProductDefined,
            Abi::LinuxArm64 | Abi::LinuxArmhf => Interpretation::Inferred,
        }
    }
}

/// Source of an ABI interpretation (7.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpretation {
    /// Defined by the standard.
    Standard,
    /// Defined by this software (7.1.1).
    ProductDefined,
    /// Not specified by the standard; inferred (7.1.2).
    Inferred,
}

#[derive(Debug, thiserror::Error)]
pub enum AbiError {
    #[error("cannot read library header: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a PE or ELF file")]
    UnknownFormat,
    /// Reported as `UNSUPPORTED_ABI` (7.3).
    #[error("unsupported ABI: {0}")]
    Unsupported(String),
}

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const EM_386: u16 = 3;
const EM_ARM: u16 = 40;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const EF_ARM_ABI_FLOAT_HARD: u32 = 0x400;

const IMAGE_FILE_MACHINE_I386: u16 = 0x014c;
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;

/// Reads the first 4 KiB of `path` and detects its ABI.
pub fn detect_file(path: &Path) -> Result<Abi, AbiError> {
    let mut header = Vec::with_capacity(4096);
    std::fs::File::open(path)?
        .take(4096)
        .read_to_end(&mut header)?;
    detect(&header)
}

/// Detects the ABI from the beginning of a library file.
pub fn detect(header: &[u8]) -> Result<Abi, AbiError> {
    if header.starts_with(ELF_MAGIC) {
        detect_elf(header)
    } else if header.starts_with(b"MZ") {
        detect_pe(header)
    } else {
        Err(AbiError::UnknownFormat)
    }
}

fn u16_at(buf: &[u8], offset: usize, little: bool) -> Option<u16> {
    let b: [u8; 2] = buf.get(offset..offset + 2)?.try_into().ok()?;
    Some(if little {
        u16::from_le_bytes(b)
    } else {
        u16::from_be_bytes(b)
    })
}

fn u32_at(buf: &[u8], offset: usize, little: bool) -> Option<u32> {
    let b: [u8; 4] = buf.get(offset..offset + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    })
}

fn detect_elf(h: &[u8]) -> Result<Abi, AbiError> {
    let class = *h.get(4).ok_or(AbiError::UnknownFormat)?; // EI_CLASS: 1 = 32-bit, 2 = 64-bit
    let little = match h.get(5) {
        Some(1) => true,
        Some(2) => false,
        _ => return Err(AbiError::UnknownFormat),
    };
    if !little {
        return Err(AbiError::Unsupported("big-endian ELF".into()));
    }
    let machine = u16_at(h, 18, little).ok_or(AbiError::UnknownFormat)?;
    match (class, machine) {
        (1, EM_386) => Ok(Abi::LinuxX86),
        (2, EM_X86_64) => Ok(Abi::LinuxX86_64),
        (2, EM_AARCH64) => Ok(Abi::LinuxArm64),
        (1, EM_ARM) => {
            // e_flags is at offset 36 in a 32-bit ELF header.
            let flags = u32_at(h, 36, little).ok_or(AbiError::UnknownFormat)?;
            if flags & EF_ARM_ABI_FLOAT_HARD != 0 {
                Ok(Abi::LinuxArmhf)
            } else {
                Err(AbiError::Unsupported("ARM soft-float ELF (armel)".into()))
            }
        }
        _ => Err(AbiError::Unsupported(format!(
            "ELF class {class}, e_machine {machine}"
        ))),
    }
}

fn detect_pe(h: &[u8]) -> Result<Abi, AbiError> {
    let pe_offset = u32_at(h, 0x3c, true).ok_or(AbiError::UnknownFormat)? as usize;
    if h.get(pe_offset..pe_offset + 4) != Some(b"PE\0\0") {
        return Err(AbiError::UnknownFormat);
    }
    match u16_at(h, pe_offset + 4, true).ok_or(AbiError::UnknownFormat)? {
        IMAGE_FILE_MACHINE_I386 => Ok(Abi::WinX86),
        IMAGE_FILE_MACHINE_AMD64 => Ok(Abi::WinX64),
        // Windows on ARM is undecided (17, P2).
        other => Err(AbiError::Unsupported(format!("PE machine {other:#06x}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf(class: u8, machine: u16, flags: u32) -> Vec<u8> {
        let mut h = vec![0u8; 64];
        h[..4].copy_from_slice(ELF_MAGIC);
        h[4] = class;
        h[5] = 1;
        h[18..20].copy_from_slice(&machine.to_le_bytes());
        h[36..40].copy_from_slice(&flags.to_le_bytes());
        h
    }

    fn pe(machine: u16) -> Vec<u8> {
        let mut h = vec![0u8; 0x100];
        h[..2].copy_from_slice(b"MZ");
        h[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        h[0x80..0x84].copy_from_slice(b"PE\0\0");
        h[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        h
    }

    #[test]
    fn default_long_sizes() {
        let eight: Vec<_> = Abi::ALL
            .into_iter()
            .filter(|abi| abi.default_long_size() == 8)
            .collect();
        assert_eq!(eight, [Abi::LinuxX86_64, Abi::LinuxArm64]);
    }

    #[test]
    fn interpretations() {
        let inferred: Vec<_> = Abi::ALL
            .into_iter()
            .filter(|abi| abi.interpretation() == Interpretation::Inferred)
            .collect();
        assert_eq!(inferred, [Abi::LinuxArm64, Abi::LinuxArmhf]);
        assert_eq!(Abi::WinX86.interpretation(), Interpretation::Standard);
        assert_eq!(
            Abi::LinuxX86_64.interpretation(),
            Interpretation::ProductDefined
        );
    }

    #[test]
    fn detects_elf() {
        assert_eq!(detect(&elf(1, EM_386, 0)).unwrap(), Abi::LinuxX86);
        assert_eq!(detect(&elf(2, EM_X86_64, 0)).unwrap(), Abi::LinuxX86_64);
        assert_eq!(detect(&elf(2, EM_AARCH64, 0)).unwrap(), Abi::LinuxArm64);
        assert_eq!(
            detect(&elf(1, EM_ARM, 0x0500_0400)).unwrap(),
            Abi::LinuxArmhf
        );
        assert!(matches!(
            detect(&elf(1, EM_ARM, 0x0500_0200)),
            Err(AbiError::Unsupported(_))
        ));
    }

    #[test]
    fn detects_pe() {
        assert_eq!(detect(&pe(IMAGE_FILE_MACHINE_I386)).unwrap(), Abi::WinX86);
        assert_eq!(detect(&pe(IMAGE_FILE_MACHINE_AMD64)).unwrap(), Abi::WinX64);
        assert!(matches!(detect(&pe(0xaa64)), Err(AbiError::Unsupported(_))));
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert!(matches!(detect(b"hello"), Err(AbiError::UnknownFormat)));
        assert!(matches!(detect(b"MZ"), Err(AbiError::UnknownFormat)));
        assert!(matches!(
            detect(&ELF_MAGIC[..]),
            Err(AbiError::UnknownFormat)
        ));
    }

    #[test]
    fn detects_own_test_binary() {
        let exe = std::env::current_exe().unwrap();
        let abi = detect_file(&exe).unwrap();
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(abi, Abi::LinuxX86_64);
        }
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(abi, Abi::WinX64);
        }
    }
}
