//! The registry view a Windows lookup reads (7.1; ADR-277 Decision item 3).
//!
//! A 64-bit Windows keeps separate copies of `HKLM\SOFTWARE` for 32-bit and 64-bit software. A
//! lookup names the view it reads instead of inheriting the caller's, so an agent can ask for the
//! view of the worker it is about to start. Outside Windows the view has no meaning and is
//! ignored.

/// Registry view of a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegistryView {
    /// The 32-bit view (`KEY_WOW64_32KEY`).
    Wow64_32,
    /// The 64-bit view (`KEY_WOW64_64KEY`).
    Wow64_64,
}

impl RegistryView {
    /// The view of this build's own bitness: the 64-bit view in a 64-bit build, the 32-bit view
    /// otherwise. (On 32-bit Windows there is only one view, and the 32-bit flag selects it.)
    pub const fn native() -> Self {
        if cfg!(target_pointer_width = "64") {
            Self::Wow64_64
        } else {
            Self::Wow64_32
        }
    }

    /// The access-mask flag that selects this view when opening a key.
    #[cfg(windows)]
    pub fn flag(self) -> u32 {
        use winreg::enums::{KEY_WOW64_32KEY, KEY_WOW64_64KEY};
        match self {
            Self::Wow64_32 => KEY_WOW64_32KEY,
            Self::Wow64_64 => KEY_WOW64_64KEY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_follows_the_pointer_width() {
        let expected = if usize::BITS == 64 {
            RegistryView::Wow64_64
        } else {
            RegistryView::Wow64_32
        };
        assert_eq!(RegistryView::native(), expected);
    }

    #[cfg(windows)]
    #[test]
    fn the_views_have_different_flags() {
        assert_ne!(RegistryView::Wow64_32.flag(), RegistryView::Wow64_64.flag());
    }

    #[cfg(windows)]
    #[test]
    fn each_view_selects_its_wow64_flag() {
        use winreg::enums::{KEY_WOW64_32KEY, KEY_WOW64_64KEY};
        assert_eq!(RegistryView::Wow64_32.flag(), KEY_WOW64_32KEY);
        assert_eq!(RegistryView::Wow64_64.flag(), KEY_WOW64_64KEY);
    }
}
