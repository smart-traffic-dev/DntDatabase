//! Byte <-> UTF-16 offset conversion for one text.

/// One non-ASCII char: where it starts in bytes and in UTF-16 units, and how
/// long it is in each. ASCII runs between two such chars map one to one.
#[derive(Debug, Clone, Copy)]
struct Wide {
    byte: usize,
    u16: u32,
    byte_len: u8,
    u16_len: u8,
}

/// Maps byte offsets of a `&str` to UTF-16 code-unit offsets and back.
///
/// Built once per text in O(n); it stores one entry per non-ASCII char (a
/// prefix sum of UTF-16 lengths), so ASCII text costs nothing and every
/// lookup is a binary search. Offsets past the end clamp to the end; an
/// offset inside a char (a UTF-8 continuation byte, or between the halves of
/// a surrogate pair) snaps to that char's start.
#[derive(Debug, Clone)]
pub struct Utf16Map {
    wide: Vec<Wide>,
    len_bytes: usize,
    len_u16: u32,
}

impl Utf16Map {
    pub fn new(text: &str) -> Self {
        let mut wide = Vec::new();
        let mut u16 = 0u32;
        for (byte, c) in text.char_indices() {
            let u16_len = c.len_utf16() as u32;
            if !c.is_ascii() {
                wide.push(Wide {
                    byte,
                    u16,
                    byte_len: c.len_utf8() as u8,
                    u16_len: u16_len as u8,
                });
            }
            u16 += u16_len;
        }
        Self {
            wide,
            len_bytes: text.len(),
            len_u16: u16,
        }
    }

    /// Length of the text in UTF-16 code units.
    pub fn len_u16(&self) -> u32 {
        self.len_u16
    }

    /// UTF-16 offset of byte offset `byte`.
    pub fn to_u16(&self, byte: usize) -> u32 {
        let byte = byte.min(self.len_bytes);
        let at = self.wide.partition_point(|w| w.byte <= byte);
        let Some(w) = at.checked_sub(1).map(|i| self.wide[i]) else {
            return byte as u32;
        };
        let end = w.byte + usize::from(w.byte_len);
        if byte < end {
            w.u16
        } else {
            w.u16 + u32::from(w.u16_len) + (byte - end) as u32
        }
    }

    /// Byte offset of UTF-16 offset `u16`; always a char boundary of the text.
    pub fn to_byte(&self, u16: u32) -> usize {
        let u16 = u16.min(self.len_u16);
        let at = self.wide.partition_point(|w| w.u16 <= u16);
        let Some(w) = at.checked_sub(1).map(|i| self.wide[i]) else {
            return u16 as usize;
        };
        let end = w.u16 + u32::from(w.u16_len);
        if u16 < end {
            w.byte
        } else {
            w.byte + usize::from(w.byte_len) + (u16 - end) as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_identity() {
        let m = Utf16Map::new("SELECT 1");
        assert_eq!(m.to_u16(3), 3);
        assert_eq!(m.to_byte(3), 3);
        assert_eq!(m.len_u16(), 8);
    }

    #[test]
    fn round_trips_every_char_boundary() {
        let text = "a ế 😀\r\nư🎉z";
        let m = Utf16Map::new(text);
        let mut u = 0u32;
        for (b, c) in text.char_indices() {
            assert_eq!(m.to_u16(b), u, "byte {b}");
            assert_eq!(m.to_byte(u), b, "u16 {u}");
            u += c.len_utf16() as u32;
        }
        assert_eq!(m.to_u16(text.len()), u);
        assert_eq!(m.to_byte(u), text.len());
        assert_eq!(m.len_u16(), u);
    }

    #[test]
    fn inside_a_char_snaps_to_its_start() {
        let text = "x😀y";
        let m = Utf16Map::new(text);
        // 😀 is bytes 1..5 and UTF-16 units 1..3.
        assert_eq!(m.to_byte(2), 1);
        assert_eq!(m.to_u16(3), 1);
        assert_eq!(m.to_byte(3), 5);
        assert_eq!(m.to_u16(5), 3);
    }

    #[test]
    fn clamps_past_the_end() {
        let m = Utf16Map::new("ế");
        assert_eq!(m.to_byte(99), 3);
        assert_eq!(m.to_u16(99), 1);
        let empty = Utf16Map::new("");
        assert_eq!(empty.to_byte(5), 0);
        assert_eq!(empty.to_u16(5), 0);
    }
}
