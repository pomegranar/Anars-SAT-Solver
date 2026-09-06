//! LEB128 variable-length integers.
//!
//! Component keys are two sorted lists of indices. Delta-encoding them and then writing the
//! deltas as varints turns a component over dense clause indices into roughly one byte per
//! clause, which matters because the memo stores every key in full.

/// Appends `value` to `out` in LEB128 form.
pub fn write_u32(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Reads a LEB128 integer from the front of `input`, returning it and the bytes consumed.
///
/// Returns `None` if the input ends mid-number or the value does not fit in a `u32`.
#[must_use]
pub fn read_u32(input: &[u8]) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut shift = 0;
    for (i, &byte) in input.iter().enumerate() {
        let payload = u32::from(byte & 0x7f);
        value |= payload.checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_across_width_boundaries() {
        let values = [0, 1, 127, 128, 129, 16_383, 16_384, 1 << 21, u32::MAX];
        let mut buf = Vec::new();
        for v in values {
            buf.clear();
            write_u32(&mut buf, v);
            assert_eq!(read_u32(&buf), Some((v, buf.len())), "value {v}");
        }
    }

    #[test]
    fn small_values_take_one_byte() {
        let mut buf = Vec::new();
        write_u32(&mut buf, 127);
        assert_eq!(buf.len(), 1);
        buf.clear();
        write_u32(&mut buf, 128);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn a_sequence_decodes_in_order() {
        let mut buf = Vec::new();
        for v in [5u32, 300, 7] {
            write_u32(&mut buf, v);
        }
        let mut rest = &buf[..];
        let mut got = Vec::new();
        while let Some((v, n)) = read_u32(rest) {
            got.push(v);
            rest = &rest[n..];
        }
        assert_eq!(got, vec![5, 300, 7]);
    }

    #[test]
    fn truncated_input_is_rejected() {
        assert_eq!(read_u32(&[]), None);
        assert_eq!(read_u32(&[0x80]), None, "continuation bit set but no follow-up byte");
    }

    #[test]
    fn overlong_encoding_is_rejected() {
        assert_eq!(read_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]), None);
    }
}
