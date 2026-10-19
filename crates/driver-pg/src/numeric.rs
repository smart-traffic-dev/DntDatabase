//! Text form of a `numeric` value in PostgreSQL's binary wire format.
//!
//! Wire format (`numeric_send` in `src/backend/utils/adt/numeric.c`), all big-endian:
//! `int16 ndigits, int16 weight, uint16 sign, int16 dscale, int16 digits[ndigits]`.
//! Digits are base 10000, the first one weighted `10000^weight`; trailing zero digits are
//! not sent. `sign` is 0x0000 (+), 0x4000 (-), 0xC000 (NaN), 0xD000 (+Inf) or 0xF000
//! (-Inf, PG 14+). `dscale` is the number of decimal digits shown after the point.

const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;

/// Formats like PostgreSQL's `numeric_out` (no exponent, exactly `dscale` fraction digits).
/// Malformed input (never sent by a server) comes back as `\x<hex>` instead of panicking.
pub(crate) fn decode_numeric(raw: &[u8]) -> String {
    decode(raw).unwrap_or_else(|| crate::values::hex_text(raw))
}

fn decode(raw: &[u8]) -> Option<String> {
    let word = |i: usize| -> Option<i16> {
        let b = raw.get(i * 2..i * 2 + 2)?;
        Some(i16::from_be_bytes([b[0], b[1]]))
    };
    let ndigits = usize::try_from(word(0)?).ok()?;
    let weight = i32::from(word(1)?);
    let sign = word(2)? as u16;
    let dscale = usize::try_from(word(3)?).ok()?;
    if raw.len() != 8 + ndigits * 2 {
        return None;
    }
    match sign {
        NUMERIC_NAN => return Some("NaN".into()),
        NUMERIC_PINF => return Some("Infinity".into()),
        NUMERIC_NINF => return Some("-Infinity".into()),
        0 | NUMERIC_NEG => {}
        _ => return None,
    }
    let digits: Vec<i16> = (0..ndigits).map(|i| word(4 + i)).collect::<Option<_>>()?;
    if digits.iter().any(|d| !(0..10_000).contains(d)) {
        return None;
    }
    // Digit at position `p` (0 = the one weighted 10000^weight); missing ones are zero.
    let digit = |p: i32| -> i16 {
        usize::try_from(p)
            .ok()
            .and_then(|p| digits.get(p).copied())
            .unwrap_or(0)
    };
    let mut out = String::new();
    if sign == NUMERIC_NEG {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        out.push_str(&digit(0).to_string());
        for p in 1..=weight {
            out.push_str(&format!("{:04}", digit(p)));
        }
    }
    if dscale > 0 {
        let mut frac = String::with_capacity(dscale + 4);
        let mut p = weight + 1;
        while frac.len() < dscale {
            frac.push_str(&format!("{:04}", digit(p)));
            p += 1;
        }
        frac.truncate(dscale);
        out.push('.');
        out.push_str(&frac);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the wire bytes from the header fields and base-10000 digits.
    fn wire(weight: i16, sign: u16, dscale: i16, digits: &[i16]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(digits.len() as i16).to_be_bytes());
        b.extend_from_slice(&weight.to_be_bytes());
        b.extend_from_slice(&sign.to_be_bytes());
        b.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            b.extend_from_slice(&d.to_be_bytes());
        }
        b
    }

    /// Vectors built by hand from the format above. Each expected string is what
    /// `SELECT '<v>'::numeric` prints, and each byte string equals
    /// `SELECT numeric_send('<v>'::numeric)`, both checked against the PG 12 and PG 17
    /// test containers (see the task report).
    #[test]
    fn decodes_hand_built_vectors() {
        // 0 -> \x0000000000000000
        let zero = wire(0, 0, 0, &[]);
        assert_eq!(hex(&zero), "0000000000000000");
        assert_eq!(decode_numeric(&zero), "0");
        // -0.5 -> \x0001ffff400000011388: one digit 5000 at weight -1, dscale 1.
        let half = wire(-1, NUMERIC_NEG, 1, &[5000]);
        assert_eq!(hex(&half), "0001ffff400000011388");
        assert_eq!(decode_numeric(&half), "-0.5");
        // 40 integer digits = 10 groups (weight 9) + .0123|4567|89(00), dscale 10.
        let big = wire(
            9,
            0,
            10,
            &[
                1234, 5678, 9012, 3456, 7890, 1234, 5678, 9012, 3456, 7890, 123, 4567, 8900,
            ],
        );
        assert_eq!(
            hex(&big),
            "000d00090000000a04d2162e23340d801ed204d2162e23340d801ed2007b11d722c4"
        );
        assert_eq!(
            decode_numeric(&big),
            "1234567890123456789012345678901234567890.0123456789"
        );
        // NaN -> \x00000000c0000000
        let nan = wire(0, NUMERIC_NAN, 0, &[]);
        assert_eq!(hex(&nan), "00000000c0000000");
        assert_eq!(decode_numeric(&nan), "NaN");
        // 0.000001 -> \x0001fffe000000060064: digit 0100 at weight -2, dscale 6.
        let micro = wire(-2, 0, 6, &[100]);
        assert_eq!(hex(&micro), "0001fffe000000060064");
        assert_eq!(decode_numeric(&micro), "0.000001");
        // 100 -> \x00010000000000000064
        let hundred = wire(0, 0, 0, &[100]);
        assert_eq!(hex(&hundred), "00010000000000000064");
        assert_eq!(decode_numeric(&hundred), "100");
    }

    #[test]
    fn trailing_zero_groups_and_scale() {
        // 1000000 -> \x00010001000000000064: the trailing 0000 group is not sent.
        assert_eq!(decode_numeric(&wire(1, 0, 0, &[100])), "1000000");
        // -12.340 -> \x0002000040000003000c0d48: dscale keeps the trailing zero.
        assert_eq!(
            decode_numeric(&wire(0, NUMERIC_NEG, 3, &[12, 3400])),
            "-12.340"
        );
    }

    #[test]
    fn infinities_and_garbage() {
        // PG 14+: numeric_send('Infinity') = \x00000000d0000020, '-Infinity' = ...f0000020.
        assert_eq!(decode_numeric(&wire(0, NUMERIC_PINF, 32, &[])), "Infinity");
        assert_eq!(decode_numeric(&wire(0, NUMERIC_NINF, 32, &[])), "-Infinity");
        // Truncated input never panics.
        assert_eq!(decode_numeric(&[0, 1, 0]), "\\x000100");
        assert_eq!(
            decode_numeric(&[0, 2, 0, 0, 0, 0, 0, 0, 0, 1]),
            "\\x00020000000000000001"
        );
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
