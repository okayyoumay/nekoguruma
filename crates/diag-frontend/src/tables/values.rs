//! The value syntax of `docs/csv-tables.md` section 3. Each parser takes a cell as written and
//! returns the value or a hint saying why it is not one.

use std::collections::HashSet;

pub(crate) type Parsed<T> = Result<T, String>;

/// An integer in `min..=max`: decimal (with `-` only when `signed`) or hexadecimal with a
/// lowercase `0x` prefix, which is an unsigned magnitude. No `+`, no spaces.
pub(crate) fn integer(cell: &str, signed: bool, min: i128, max: i128) -> Parsed<i128> {
    let value = if let Some(hex) = cell.strip_prefix("0x") {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{cell:?} is not a hexadecimal integer"));
        }
        // More than 32 significant digits cannot fit any column; trim leading zeros first.
        let digits = hex.trim_start_matches('0');
        if digits.len() > 32 {
            return Err(format!("{cell} is out of range ({min} to {max})"));
        }
        if digits.is_empty() {
            0
        } else {
            let magnitude = u128::from_str_radix(digits, 16)
                .map_err(|_| format!("{cell:?} is not a hexadecimal integer"))?;
            i128::try_from(magnitude)
                .map_err(|_| format!("{cell} is out of range ({min} to {max})"))?
        }
    } else {
        let (negative, digits) = match cell.strip_prefix('-') {
            Some(rest) if signed => (true, rest),
            Some(_) => return Err(format!("{cell:?}: this column takes no sign")),
            None => (false, cell),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("{cell:?} is not an integer"));
        }
        let digits = digits.trim_start_matches('0');
        if digits.len() > 38 {
            return Err(format!("{cell} is out of range ({min} to {max})"));
        }
        let magnitude: i128 = if digits.is_empty() {
            0
        } else {
            digits
                .parse()
                .map_err(|_| format!("{cell:?} is not an integer"))?
        };
        if negative { -magnitude } else { magnitude }
    };
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{cell} is out of range ({min} to {max})"))
    }
}

/// A finite decimal float: an optional `-`, digits, an optional fraction with digits on both
/// sides of the `.`, and an optional exponent.
pub(crate) fn float(cell: &str) -> Parsed<f64> {
    let bad = || format!("{cell:?} is not a decimal number");
    let body = cell.strip_prefix('-').unwrap_or(cell);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (mantissa, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || fraction.is_some_and(|f| !digits(f)) {
        return Err(bad());
    }
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        if !digits(exponent) {
            return Err(bad());
        }
    }
    let value: f64 = cell.parse().map_err(|_| bad())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("{cell} is not finite in double precision"))
    }
}

/// Hexadecimal digit pairs, upper or lower case, with optional single spaces between pairs.
pub(crate) fn bytes(cell: &str) -> Parsed<Vec<u8>> {
    let bad = || format!("{cell:?} is not hexadecimal byte pairs");
    let mut out = Vec::new();
    let mut rest = cell.as_bytes();
    loop {
        let [high, low, tail @ ..] = rest else {
            return Err(bad());
        };
        let pair = std::str::from_utf8(&[*high, *low])
            .ok()
            .filter(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|p| u8::from_str_radix(p, 16).ok())
            .ok_or_else(bad)?;
        out.push(pair);
        rest = match tail {
            [] => return Ok(out),
            [b' ', after @ ..] => after,
            _ => tail,
        };
    }
}

/// `yes` or `no`.
pub(crate) fn boolean(cell: &str) -> Parsed<bool> {
    match cell {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(format!("{cell:?} is not yes or no")),
    }
}

fn is_name(cell: &str) -> bool {
    let mut bytes = cell.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
pub(crate) fn name(cell: &str) -> Parsed<String> {
    if is_name(cell) {
        Ok(cell.to_owned())
    } else {
        Err(format!("{cell:?} is not a name"))
    }
}

/// Names joined by `.`.
pub(crate) fn key(cell: &str) -> Parsed<String> {
    if cell.split('.').all(is_name) {
        Ok(cell.to_owned())
    } else {
        Err(format!("{cell:?} is not a key"))
    }
}

/// A list: elements joined by `;`, none empty; with `unique`, none given twice.
pub(crate) fn list<T>(
    cell: &str,
    unique: bool,
    element: impl Fn(&str) -> Parsed<T>,
) -> Parsed<Vec<T>> {
    let mut seen = HashSet::new();
    cell.split(';')
        .map(|item| {
            if item.is_empty() {
                return Err(format!("{cell:?} has an empty element"));
            }
            if item.starts_with(char::is_whitespace) || item.ends_with(char::is_whitespace) {
                return Err(format!("{cell:?}: list elements are joined without spaces"));
            }
            if unique && !seen.insert(item) {
                return Err(format!("{item:?} is given twice"));
            }
            element(item)
        })
        .collect()
}

/// Any text.
pub(crate) fn text(cell: &str) -> Parsed<String> {
    Ok(cell.to_owned())
}

/// Which generated service of a DID or routine a service name means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Generated {
    /// `<did>.read`.
    Read,
    /// `<did>.write`.
    Write,
    /// `<routine>.start`.
    Start,
    /// `<routine>.stop`.
    Stop,
    /// `<routine>.results`.
    Results,
}

/// A service name: a name, or a DID or routine name with the generated service's suffix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceName {
    /// The service's name, or the DID's or routine's.
    pub name: String,
    /// The generated service, if this names one.
    pub generated: Option<Generated>,
}

pub(crate) fn service_name(cell: &str) -> Parsed<ServiceName> {
    let bad = || format!("{cell:?} is not a service name");
    match cell.split_once('.') {
        None => Ok(ServiceName {
            name: name(cell).map_err(|_| bad())?,
            generated: None,
        }),
        Some((base, suffix)) => {
            let generated = match suffix {
                "read" => Generated::Read,
                "write" => Generated::Write,
                "start" => Generated::Start,
                "stop" => Generated::Stop,
                "results" => Generated::Results,
                _ => return Err(bad()),
            };
            Ok(ServiceName {
                name: name(base).map_err(|_| bad())?,
                generated: Some(generated),
            })
        }
    }
}

/// One of a fixed set of keywords.
pub(crate) fn keyword<T: Copy>(cell: &str, words: &[(&str, T)]) -> Parsed<T> {
    words
        .iter()
        .find(|(word, _)| *word == cell)
        .map(|(_, value)| *value)
        .ok_or_else(|| {
            let names: Vec<&str> = words.iter().map(|(w, _)| *w).collect();
            format!("{cell:?} is not one of {}", names.join(", "))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const U32: (i128, i128) = (0, u32::MAX as i128);

    #[test]
    fn integers_are_decimal_or_lowercase_hex() {
        let u = |c: &str| integer(c, false, U32.0, U32.1);
        assert_eq!(u("500000"), Ok(500_000));
        assert_eq!(u("0x7E0"), Ok(0x7E0));
        assert_eq!(u("0x7e0"), Ok(0x7E0));
        assert_eq!(u("007"), Ok(7));
        assert_eq!(u("0x0000"), Ok(0));
        assert_eq!(u("0xFFFFFFFF"), Ok(u32::MAX.into()));
        for bad in [
            "",
            "0X10",
            "0x",
            "+5",
            " 5",
            "5 ",
            "-5",
            "1e3",
            "0x1G",
            "4294967296",
        ] {
            assert!(u(bad).is_err(), "{bad:?}");
        }
        let i = |c: &str| integer(c, true, i64::MIN.into(), i64::MAX.into());
        assert_eq!(i("-9223372036854775808"), Ok(i64::MIN.into()));
        assert_eq!(i("-0"), Ok(0));
        // Hexadecimal is a magnitude: all ones is out of a signed range, not -1.
        assert!(i("0xFFFFFFFFFFFFFFFF").is_err());
        assert!(i("-0x1").is_err());
        let wide = |c: &str| integer(c, false, 0, u64::MAX.into());
        assert_eq!(wide("0xFFFFFFFFFFFFFFFF"), Ok(u64::MAX.into()));
        assert!(wide(&format!("0x{}", "F".repeat(40))).is_err());
        assert!(wide(&"9".repeat(60)).is_err());
    }

    #[test]
    fn floats_need_digits_on_both_sides_of_the_point() {
        for (cell, value) in [("0.1", 0.1), ("1e-3", 1e-3), ("-2", -2.0), ("5E+2", 500.0)] {
            assert_eq!(float(cell), Ok(value), "{cell}");
        }
        for bad in [
            ".5", "5.", "0x10", "inf", "nan", "1e999", "+1", "1e", "", " 1", "1.2.3",
        ] {
            assert!(float(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bytes_are_hex_pairs_with_single_spaces() {
        assert_eq!(bytes("F190"), Ok(vec![0xF1, 0x90]));
        assert_eq!(bytes("31 2e"), Ok(vec![0x31, 0x2E]));
        for bad in [
            "", "F", "F19", "F1  90", " F1", "F1 ", "0xF1", "GG", "F1,90",
        ] {
            assert!(bytes(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names_keys_and_service_names() {
        assert!(name("softwareVersion").is_ok());
        assert!(name("_a1").is_ok());
        for bad in ["", "1a", "a.b", "a-b", "a b"] {
            assert!(name(bad).is_err(), "{bad:?}");
        }
        assert!(key("session.default").is_ok());
        for bad in ["", "a.", ".a", "a..b", "a.1"] {
            assert!(key(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            service_name("vin.read"),
            Ok(ServiceName {
                name: "vin".into(),
                generated: Some(Generated::Read)
            })
        );
        assert_eq!(service_name("reset").map(|s| s.generated), Ok(None));
        for bad in ["vin.erase", "vin.read.x", ".read", "a-b"] {
            assert!(service_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lists_reject_empty_and_repeated_elements() {
        assert_eq!(
            list("1.0.0;1.0.1", true, text),
            Ok(vec!["1.0.0".into(), "1.0.1".into()])
        );
        for bad in [";a", "a;", "a;;b", ""] {
            assert!(list(bad, false, text).is_err(), "{bad:?}");
        }
        assert!(list("a;a", true, text).is_err());
        for bad in ["1.0.0; 1.0.1", "1.0.0 ;1.0.1", " a"] {
            assert!(list(bad, false, text).is_err(), "{bad:?}");
        }
        // A space inside an element is part of it.
        assert_eq!(
            list("NGR EX;b", false, text),
            Ok(vec!["NGR EX".into(), "b".into()])
        );
        assert_eq!(list("1;1", false, float), Ok(vec![1.0, 1.0]));
        assert!(boolean("Yes").is_err());
    }
}
