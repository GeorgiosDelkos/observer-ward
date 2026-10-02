//! Kubernetes resource `Quantity` parsing.
//!
//! The grammar (`k8s.io/apimachinery/pkg/api/resource`) is
//! `<number><suffix>`, where the suffix is a binary SI unit (`Ki`..`Ei`),
//! a decimal SI unit (`n`, `u`, `m`, none, `k`, `M`..`E`), or a decimal
//! exponent (`e3`, `E-2`). Any suffix can apply to any resource: the API
//! server canonicalises a memory limit of `1.2Gi` to `1288490188800m`
//! (milli-bytes), so a memory parser that only knows byte units silently
//! drops real limits.

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use super::error::QuantityParseError;

/// How a suffix scales the number in front of it.
#[derive(Debug, Clone, Copy)]
enum Scale {
    /// Multiply by `2^n`.
    Binary(i32),
    /// Multiply by `10^n`.
    Decimal(i32),
}

impl Scale {
    /// Negative powers of ten divide rather than multiply by a fraction.
    /// `10^n` is exact in f64 and division is correctly rounded, so a whole
    /// multiple such as `3000m` comes back exactly 3; multiplying by the
    /// inexact `0.001` gives no such guarantee, and `ceil` would turn a
    /// result one ulp above 3 into 4 bytes.
    fn apply(self, number: f64) -> f64 {
        match self {
            Scale::Binary(exp) => number * 2_f64.powi(exp),
            Scale::Decimal(exp) if exp >= 0 => number * 10_f64.powi(exp),
            Scale::Decimal(exp) => number / 10_f64.powi(-exp),
        }
    }
}

/// The scale for a quantity suffix, or `None` if it is not one.
fn suffix_scale(suffix: &str) -> Option<Scale> {
    let scale = match suffix {
        "Ki" => Scale::Binary(10),
        "Mi" => Scale::Binary(20),
        "Gi" => Scale::Binary(30),
        "Ti" => Scale::Binary(40),
        "Pi" => Scale::Binary(50),
        "Ei" => Scale::Binary(60),
        "n" => Scale::Decimal(-9),
        "u" => Scale::Decimal(-6),
        "m" => Scale::Decimal(-3),
        "" => Scale::Decimal(0),
        "k" => Scale::Decimal(3),
        "M" => Scale::Decimal(6),
        "G" => Scale::Decimal(9),
        "T" => Scale::Decimal(12),
        "P" => Scale::Decimal(15),
        "E" => Scale::Decimal(18),
        // `e<int>` / `E<int>`; a bare `E` is the exa suffix above.
        _ => {
            let exponent = suffix.strip_prefix(['e', 'E'])?;
            Scale::Decimal(exponent.parse().ok()?)
        }
    };
    Some(scale)
}

/// Parse a quantity into its value in base units (cores, bytes).
///
/// Values are carried as `f64`: exact for integers up to 2^53 (8 PiB),
/// which covers any real CPU or memory figure.
fn parse_quantity(q: &Quantity) -> Result<f64, QuantityParseError> {
    let s = q.0.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '+' | '-')))
        .unwrap_or(s.len());
    let (number, suffix) = s.split_at(split);

    let scale =
        suffix_scale(suffix).ok_or_else(|| QuantityParseError::Suffix { value: q.0.clone() })?;
    let number: f64 = number
        .parse()
        .map_err(|source| QuantityParseError::Number {
            value: q.0.clone(),
            source,
        })?;
    if number < 0.0 {
        return Err(QuantityParseError::Negative { value: q.0.clone() });
    }

    Ok(scale.apply(number))
}

/// Parse a CPU quantity to fractional cores (`250m` -> 0.25).
pub(super) fn parse_cpu_quantity(q: &Quantity) -> Result<f64, QuantityParseError> {
    parse_quantity(q)
}

/// Parse a memory quantity to bytes. Fractional bytes round up, as
/// Kubernetes' own `Quantity.Value()` does.
pub(super) fn parse_memory_quantity(q: &Quantity) -> Result<u64, QuantityParseError> {
    let bytes = parse_quantity(q)?.ceil();
    // u64::MAX is not representable in f64; 2^64 is the first value past it.
    if !bytes.is_finite() || bytes >= 2_f64.powi(64) {
        return Err(QuantityParseError::Overflow { value: q.0.clone() });
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "range-checked above: finite, non-negative, below 2^64"
    )]
    Ok(bytes as u64)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    use proptest::prelude::*;

    fn q(s: &str) -> Quantity {
        Quantity(s.to_string())
    }

    fn cpu(s: &str) -> f64 {
        parse_cpu_quantity(&q(s)).unwrap()
    }

    fn mem(s: &str) -> u64 {
        parse_memory_quantity(&q(s)).unwrap()
    }

    fn assert_f64_near(left: f64, right: f64) {
        assert!((left - right).abs() < 1e-9, "expected ~{right}, got {left}");
    }

    #[test]
    fn cpu_units() {
        assert_f64_near(cpu("250000000n"), 0.25);
        assert_f64_near(cpu("1000000000n"), 1.0);
        assert_f64_near(cpu("100u"), 0.0001);
        assert_f64_near(cpu("250m"), 0.25);
        assert_f64_near(cpu("1000m"), 1.0);
        assert_f64_near(cpu("4"), 4.0);
        assert_f64_near(cpu("0.5"), 0.5);
        assert_f64_near(cpu("0n"), 0.0);
        assert_f64_near(cpu("2e-1"), 0.2);
    }

    #[test]
    fn memory_binary_units() {
        assert_eq!(mem("1024Ki"), 1024 * 1024);
        assert_eq!(mem("512Mi"), 512 * 1024 * 1024);
        assert_eq!(mem("8Gi"), 8 * 1024 * 1024 * 1024);
        assert_eq!(mem("1Ti"), 1024 * 1024 * 1024 * 1024);
        assert_eq!(mem("1.5Gi"), 1_610_612_736);
    }

    #[test]
    fn memory_decimal_units_and_exponents() {
        assert_eq!(mem("4096"), 4096);
        assert_eq!(mem("0"), 0);
        assert_eq!(mem("500k"), 500_000);
        assert_eq!(mem("256M"), 256_000_000);
        assert_eq!(mem("1.5M"), 1_500_000);
        assert_eq!(mem("2G"), 2_000_000_000);
        assert_eq!(mem("1T"), 1_000_000_000_000);
        assert_eq!(mem("129e6"), 129_000_000);
        assert_eq!(mem("1E3"), 1000);
        assert_eq!(mem("2E"), 2_000_000_000_000_000_000);
    }

    #[test]
    fn memory_milli_bytes_from_api_canonicalisation() {
        // The API server stores a `1.2Gi` limit as milli-bytes.
        assert_eq!(mem("1288490188800m"), 1_288_490_189);
        // Fractional bytes round up.
        assert_eq!(mem("1500m"), 2);
    }

    #[test]
    fn invalid_quantities_are_rejected() {
        for bad in [
            "", "abc", "abcm", "badMi", "1.5.5", "1Xi", "1e", "--1", "Mi",
        ] {
            assert!(parse_memory_quantity(&q(bad)).is_err(), "{bad:?}");
            assert!(parse_cpu_quantity(&q(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn negative_and_overflowing_memory_is_rejected() {
        let err = parse_memory_quantity(&q("-1Gi")).unwrap_err();
        assert!(
            matches!(err, QuantityParseError::Negative { .. }),
            "{err:?}"
        );

        let err = parse_memory_quantity(&q("16Ei")).unwrap_err();
        assert!(
            matches!(err, QuantityParseError::Overflow { .. }),
            "{err:?}"
        );
    }

    const SUFFIXES: [(&str, u64); 11] = [
        ("", 1),
        ("k", 1_000),
        ("M", 1_000_000),
        ("G", 1_000_000_000),
        ("T", 1_000_000_000_000),
        ("Ki", 1 << 10),
        ("Mi", 1 << 20),
        ("Gi", 1 << 30),
        ("Ti", 1 << 40),
        ("e3", 1_000),
        ("e6", 1_000_000),
    ];

    proptest! {
        #[test]
        fn parsers_never_panic(s in ".*") {
            let _ = parse_cpu_quantity(&q(&s));
            let _ = parse_memory_quantity(&q(&s));
        }

        // Any integer with any whole-unit suffix parses exactly, as long
        // as the byte count stays within f64's exact-integer range.
        #[test]
        fn integer_quantities_parse_exactly(
            n in 0_u64..=u64::from(u32::MAX),
            (suffix, factor) in proptest::sample::select(SUFFIXES.to_vec()),
        ) {
            prop_assume!(n.checked_mul(factor).is_some_and(|b| b < 1 << 53));
            prop_assert_eq!(mem(&format!("{n}{suffix}")), n * factor);
        }

        // Milli-bytes round up to whole bytes, never down.
        #[test]
        fn milli_bytes_round_up(millis in 0_u64..1 << 40) {
            prop_assert_eq!(mem(&format!("{millis}m")), millis.div_ceil(1000));
        }

        // Whole bytes written as milli-bytes stay exact.
        #[test]
        fn whole_milli_bytes_are_exact(bytes in 0_u64..1 << 40) {
            prop_assert_eq!(mem(&format!("{}m", bytes * 1000)), bytes);
        }
    }
}
