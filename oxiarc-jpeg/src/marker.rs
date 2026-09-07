//! JPEG marker codes (ITU-T T.81 Table B.1).
//!
//! A marker is the two-byte sequence `0xFF` followed by a code in
//! `0x01..=0xFE`. `0xFF 0x00` is a stuffed literal `0xFF` inside entropy-coded
//! data and is never a marker; any number of `0xFF` fill bytes may precede a
//! marker code.

/// A JPEG marker code (the byte following `0xFF`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Marker {
    /// Start of image.
    Soi,
    /// End of image.
    Eoi,
    /// Start of frame; the payload byte is the marker code (`0xC0..=0xCF`).
    Sof(u8),
    /// Define Huffman table(s).
    Dht,
    /// Define arithmetic coding conditioning(s).
    Dac,
    /// Restart with modulo-8 count `n` (`RST0..RST7`).
    Rst(u8),
    /// Start of scan.
    Sos,
    /// Define quantisation table(s).
    Dqt,
    /// Define number of lines.
    Dnl,
    /// Define restart interval.
    Dri,
    /// Define hierarchical progression.
    Dhp,
    /// Expand reference component(s).
    Exp,
    /// Application segment `n` (`APP0..APP15`).
    App(u8),
    /// Comment.
    Com,
    /// Temporary use in arithmetic coding.
    Tem,
    /// Any other reserved code (`JPGn`, `RESn`).
    Reserved(u8),
}

impl Marker {
    /// Build a marker from the code byte that follows `0xFF`.
    ///
    /// Returns `None` for `0x00` (a stuffed byte) and `0xFF` (a fill byte),
    /// neither of which is a marker code.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_jpeg::Marker;
    ///
    /// assert_eq!(Marker::from_code(0xD8), Some(Marker::Soi));
    /// assert_eq!(Marker::from_code(0xC0), Some(Marker::Sof(0xC0)));
    /// assert_eq!(Marker::from_code(0x00), None);
    /// ```
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Marker> {
        match code {
            0x00 | 0xFF => None,
            0x01 => Some(Marker::Tem),
            0xC4 => Some(Marker::Dht),
            0xC8 => Some(Marker::Reserved(0xC8)),
            0xCC => Some(Marker::Dac),
            0xC0..=0xCF => Some(Marker::Sof(code)),
            0xD0..=0xD7 => Some(Marker::Rst(code - 0xD0)),
            0xD8 => Some(Marker::Soi),
            0xD9 => Some(Marker::Eoi),
            0xDA => Some(Marker::Sos),
            0xDB => Some(Marker::Dqt),
            0xDC => Some(Marker::Dnl),
            0xDD => Some(Marker::Dri),
            0xDE => Some(Marker::Dhp),
            0xDF => Some(Marker::Exp),
            0xE0..=0xEF => Some(Marker::App(code - 0xE0)),
            0xFE => Some(Marker::Com),
            other => Some(Marker::Reserved(other)),
        }
    }

    /// The code byte that follows `0xFF` for this marker.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Marker::Soi => 0xD8,
            Marker::Eoi => 0xD9,
            Marker::Sof(code) => code,
            Marker::Dht => 0xC4,
            Marker::Dac => 0xCC,
            Marker::Rst(n) => 0xD0 + n,
            Marker::Sos => 0xDA,
            Marker::Dqt => 0xDB,
            Marker::Dnl => 0xDC,
            Marker::Dri => 0xDD,
            Marker::Dhp => 0xDE,
            Marker::Exp => 0xDF,
            Marker::App(n) => 0xE0 + n,
            Marker::Com => 0xFE,
            Marker::Tem => 0x01,
            Marker::Reserved(code) => code,
        }
    }

    /// The full 16-bit marker (`0xFFxx`), as used in error messages.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        0xFF00 | self.code() as u16
    }

    /// `true` when the marker is a standalone code with no length field.
    ///
    /// `SOI`, `EOI`, `TEM` and the eight `RSTn` markers carry no payload;
    /// every other marker is followed by a two-byte big-endian length that
    /// includes those two bytes.
    #[must_use]
    pub const fn is_standalone(self) -> bool {
        matches!(
            self,
            Marker::Soi | Marker::Eoi | Marker::Tem | Marker::Rst(_)
        )
    }

    /// `true` for the ten `SOF` codes this crate can decode or name.
    #[must_use]
    pub const fn is_sof(self) -> bool {
        matches!(self, Marker::Sof(_))
    }
}

/// Marker code for `SOI`.
pub(crate) const SOI: u8 = 0xD8;
/// Marker code for `EOI`.
pub(crate) const EOI: u8 = 0xD9;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_code() {
        for code in 0x01u8..=0xFEu8 {
            if code == 0xFF {
                continue;
            }
            let marker = Marker::from_code(code).expect("non-zero code is a marker");
            assert_eq!(marker.code(), code, "code {code:#04X} did not round-trip");
        }
    }

    #[test]
    fn stuffing_and_fill_are_not_markers() {
        assert_eq!(Marker::from_code(0x00), None);
        assert_eq!(Marker::from_code(0xFF), None);
    }

    #[test]
    fn sof_codes_exclude_dht_dac_and_jpg() {
        assert_eq!(Marker::from_code(0xC4), Some(Marker::Dht));
        assert_eq!(Marker::from_code(0xC8), Some(Marker::Reserved(0xC8)));
        assert_eq!(Marker::from_code(0xCC), Some(Marker::Dac));
        for code in [0xC0u8, 0xC1, 0xC2, 0xC3, 0xC5, 0xC9, 0xCA, 0xCF] {
            assert!(
                Marker::from_code(code).expect("marker").is_sof(),
                "{code:#04X} should be a SOF"
            );
        }
    }

    #[test]
    fn standalone_markers_have_no_length() {
        assert!(Marker::Soi.is_standalone());
        assert!(Marker::Eoi.is_standalone());
        assert!(Marker::Tem.is_standalone());
        assert!(Marker::Rst(3).is_standalone());
        assert!(!Marker::Sos.is_standalone());
        assert!(!Marker::App(0).is_standalone());
    }

    #[test]
    fn as_u16_is_the_full_marker() {
        assert_eq!(Marker::Soi.as_u16(), 0xFFD8);
        assert_eq!(Marker::App(14).as_u16(), 0xFFEE);
    }
}
