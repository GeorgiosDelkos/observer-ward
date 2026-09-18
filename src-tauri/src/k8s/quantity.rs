//! Kubernetes resource quantity parsers.

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use super::error::QuantityParseError;

// -- Quantity parsers --

/// Parse a Kubernetes CPU `Quantity` to fractional cores.
///
/// Handles nanocores ("100n"), millicores ("250m"), and whole
/// cores ("2").
pub(super) fn parse_cpu_quantity(q: &Quantity) -> Result<f64, QuantityParseError> {
    let s = &q.0;
    let cpu_err = |source| QuantityParseError::Cpu {
        value: s.clone(),
        source,
    };
    if let Some(v) = s.strip_suffix('n') {
        v.parse::<f64>()
            .map(|n| n / 1_000_000_000.0)
            .map_err(cpu_err)
    } else if let Some(v) = s.strip_suffix('u') {
        v.parse::<f64>().map(|n| n / 1_000_000.0).map_err(cpu_err)
    } else if let Some(v) = s.strip_suffix('m') {
        v.parse::<f64>().map(|n| n / 1000.0).map_err(cpu_err)
    } else {
        s.parse::<f64>().map_err(cpu_err)
    }
}

/// Parse a Kubernetes memory `Quantity` to bytes.
///
/// Handles binary suffixes (Ki, Mi, Gi, Ti), decimal suffixes
/// (k, M, G, T), exponent notation (e.g. "129e6"), and plain
/// byte values.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "memory quantities from K8s are always non-negative \
              and fit in u64"
)]
pub(super) fn parse_memory_quantity(q: &Quantity) -> Result<u64, QuantityParseError> {
    let s = &q.0;
    // Both closures capture `s` by shared reference, so they are `Copy`
    // and may be reused across the loops below.
    let int_err = |source| QuantityParseError::MemoryInt {
        value: s.clone(),
        source,
    };
    let float_err = |source| QuantityParseError::MemoryFloat {
        value: s.clone(),
        source,
    };

    // Binary suffixes (check before decimal — "Mi" before "M").
    // `checked_mul` converts an overflowing product into a typed error
    // rather than a debug-build panic / release-build silent wrap
    // (axiom `rust_quality_113`). The factors 2^10..2^60 fit in u64; the
    // product `n * factor` may not, which is exactly what we guard.
    for (suffix, factor) in [
        ("Ki", 1_u64 << 10),
        ("Mi", 1_u64 << 20),
        ("Gi", 1_u64 << 30),
        ("Ti", 1_u64 << 40),
        ("Pi", 1_u64 << 50),
        ("Ei", 1_u64 << 60),
    ] {
        if let Some(v) = s.strip_suffix(suffix) {
            let n: u64 = v.parse().map_err(int_err)?;
            return n
                .checked_mul(factor)
                .ok_or_else(|| QuantityParseError::MemoryOverflow { value: s.clone() });
        }
    }

    // Decimal suffixes — parse as f64 to support fractional values like
    // "1.5G". The f64 -> u64 cast saturates (Rust 1.45+), so an enormous
    // value clamps to u64::MAX instead of wrapping.
    for (suffix, factor) in [
        ('k', 1e3_f64),
        ('M', 1e6),
        ('G', 1e9),
        ('T', 1e12),
        ('P', 1e15),
        ('E', 1e18),
    ] {
        if let Some(v) = s.strip_suffix(suffix) {
            return v
                .parse::<f64>()
                .map(|n| (n * factor) as u64)
                .map_err(float_err);
        }
    }

    // Plain bytes or exponent notation (e.g. "4096", "129e6").
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    s.parse::<f64>().map(|n| n as u64).map_err(float_err)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    fn q(s: &str) -> Quantity {
        Quantity(s.to_string())
    }

    fn assert_f64_near(left: f64, right: f64, epsilon: f64) {
        assert!(
            (left - right).abs() < epsilon,
            "expected ~{right}, got {left}"
        );
    }

    #[test]
    fn cpu_nanocores() {
        let result = parse_cpu_quantity(&q("250000000n")).unwrap();
        assert_f64_near(result, 0.25, 1e-9);
    }

    #[test]
    fn cpu_nanocores_one_core() {
        let result = parse_cpu_quantity(&q("1000000000n")).unwrap();
        assert_f64_near(result, 1.0, 1e-9);
    }

    #[test]
    fn cpu_microcores() {
        let result = parse_cpu_quantity(&q("100u")).unwrap();
        assert_f64_near(result, 0.0001, 1e-12);
    }

    #[test]
    fn cpu_millicores() {
        let result = parse_cpu_quantity(&q("250m")).unwrap();
        assert_f64_near(result, 0.25, 1e-9);
    }

    #[test]
    fn cpu_millicores_full_core() {
        let result = parse_cpu_quantity(&q("1000m")).unwrap();
        assert_f64_near(result, 1.0, 1e-9);
    }

    #[test]
    fn cpu_whole_cores() {
        let result = parse_cpu_quantity(&q("4")).unwrap();
        assert_f64_near(result, 4.0, 1e-9);
    }

    #[test]
    fn cpu_fractional_cores() {
        let result = parse_cpu_quantity(&q("0.5")).unwrap();
        assert_f64_near(result, 0.5, 1e-9);
    }

    #[test]
    fn cpu_invalid_value() {
        let err = parse_cpu_quantity(&q("abcm")).unwrap_err();
        assert!(matches!(err, QuantityParseError::Cpu { .. }), "{err:?}");
    }

    #[test]
    fn cpu_invalid_plain() {
        let result = parse_cpu_quantity(&q("xyz"));
        assert!(result.is_err());
    }

    #[test]
    fn cpu_zero_nanocores() {
        let result = parse_cpu_quantity(&q("0n")).unwrap();
        assert_f64_near(result, 0.0, 1e-9);
    }

    // -- Memory quantity parsing --

    #[test]
    fn memory_kibibytes() {
        let result = parse_memory_quantity(&q("1024Ki")).unwrap();
        assert_eq!(result, 1024 * 1024);
    }

    #[test]
    fn memory_mebibytes() {
        let result = parse_memory_quantity(&q("512Mi")).unwrap();
        assert_eq!(result, 512 * 1024 * 1024);
    }

    #[test]
    fn memory_gibibytes() {
        let result = parse_memory_quantity(&q("8Gi")).unwrap();
        assert_eq!(result, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn memory_tebibytes() {
        let result = parse_memory_quantity(&q("1Ti")).unwrap();
        assert_eq!(result, 1024 * 1024 * 1024 * 1024);
    }

    #[test]
    fn memory_plain_bytes() {
        let result = parse_memory_quantity(&q("4096")).unwrap();
        assert_eq!(result, 4096);
    }

    #[test]
    fn memory_zero() {
        let result = parse_memory_quantity(&q("0")).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn memory_invalid_value() {
        let err = parse_memory_quantity(&q("badMi")).unwrap_err();
        assert!(
            matches!(err, QuantityParseError::MemoryInt { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn memory_invalid_plain() {
        let result = parse_memory_quantity(&q("notanumber"));
        assert!(result.is_err());
    }

    // -- Decimal memory suffixes --

    #[test]
    fn memory_decimal_kilo() {
        let result = parse_memory_quantity(&q("500k")).unwrap();
        assert_eq!(result, 500_000);
    }

    #[test]
    fn memory_decimal_mega() {
        let result = parse_memory_quantity(&q("256M")).unwrap();
        assert_eq!(result, 256_000_000);
    }

    #[test]
    fn memory_decimal_mega_fractional() {
        let result = parse_memory_quantity(&q("1.5M")).unwrap();
        assert_eq!(result, 1_500_000);
    }

    #[test]
    fn memory_decimal_giga() {
        let result = parse_memory_quantity(&q("2G")).unwrap();
        assert_eq!(result, 2_000_000_000);
    }

    #[test]
    fn memory_decimal_tera() {
        let result = parse_memory_quantity(&q("1T")).unwrap();
        assert_eq!(result, 1_000_000_000_000);
    }

    #[test]
    fn memory_exponent_notation() {
        let result = parse_memory_quantity(&q("129e6")).unwrap();
        assert_eq!(result, 129_000_000);
    }

    use proptest::prelude::*;

    proptest! {
        // Quantity parsers must never panic on arbitrary input; the
        // checked/saturating arithmetic guards the overflow paths.
        #[test]
        fn quantity_parsers_never_panic(s in ".*") {
            let _ = parse_cpu_quantity(&q(&s));
            let _ = parse_memory_quantity(&q(&s));
        }

        // Binary `Ki` quantities round-trip exactly for any u32 magnitude.
        #[test]
        fn memory_ki_roundtrip(n in 0u64..=u64::from(u32::MAX)) {
            let parsed = parse_memory_quantity(&q(&format!("{n}Ki"))).unwrap();
            prop_assert_eq!(parsed, n * 1024);
        }
    }
}
