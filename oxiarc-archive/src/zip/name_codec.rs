//! ZIP entry-name and comment decoding.
//!
//! The PKWARE APPNOTE (Appendix D) specifies that ZIP entry names are
//! encoded in IBM code page 437 unless general-purpose flag bit 11 (the
//! "language encoding flag", EFS) marks them as UTF-8. In practice,
//! archives produced on Japanese Windows store names in Shift_JIS
//! (Windows-31J) without setting the EFS flag, and archives produced on
//! Unix-like systems store UTF-8 without setting it either.
//!
//! Decoding such names with `String::from_utf8_lossy` collapses every
//! non-ASCII byte pair into U+FFFD, so distinct names such as
//! `あ.txt` and `い.txt` become identical and extraction silently
//! overwrites files. This module restores the original names with a
//! discriminating decode chain that keeps distinct raw byte strings
//! distinct:
//!
//! 1. strict UTF-8 (always correct when it succeeds; mandated when the
//!    EFS flag is set),
//! 2. Shift_JIS (the de-facto standard of Japanese-Windows ZIPs) — only
//!    attempted when the EFS flag is *not* set,
//! 3. code page 437, which is injective (each byte maps to a unique
//!    character), as the final fallback so no information is ever lost.

use encoding_rs::SHIFT_JIS;

/// Mapping of CP437 bytes `0x80..=0xFF` to Unicode.
///
/// Bytes below 0x80 map to themselves (ASCII). The table below matches the
/// classic IBM PC code page 437 and yields an injective decode: distinct
/// raw byte strings always produce distinct decoded strings.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{00A0}',
];

/// Decode a byte string as CP437 (injective: distinct inputs always
/// produce distinct outputs, and no U+FFFD is ever emitted).
fn decode_cp437(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                CP437_HIGH[(b - 0x80) as usize]
            }
        })
        .collect()
}

/// Decode a ZIP entry name (or comment) from its raw header bytes.
///
/// * `utf8_flag` — the EFS bit (general-purpose flag bit 11) of the entry.
///
/// Decode chain:
///
/// - strict UTF-8 first (this also covers the EFS case and the common
///   Unix-produced archive without EFS),
/// - if the EFS flag is not set and the bytes are not valid UTF-8, try
///   Shift_JIS (the norm for Japanese-Windows ZIPs),
/// - otherwise fall back to CP437, which is injective, so distinct raw
///   names can never collapse into the same decoded name.
pub(crate) fn decode_zip_text(bytes: &[u8], utf8_flag: bool) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    if !utf8_flag {
        let (decoded, _, had_errors) = SHIFT_JIS.decode(bytes);
        if !had_errors {
            return decoded.into_owned();
        }
    }
    decode_cp437(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_jis_names_without_efs_decode_to_distinct_japanese() {
        // Shift_JIS encodings of "あ.txt" and "い.txt".
        let a = [0x82, 0xA0, b'.', b't', b'x', b't'];
        let i = [0x82, 0xA2, b'.', b't', b'x', b't'];
        assert_eq!(decode_zip_text(&a, false), "あ.txt");
        assert_eq!(decode_zip_text(&i, false), "い.txt");
    }

    #[test]
    fn utf8_names_decode_with_and_without_efs() {
        // EFS flag set: UTF-8 as mandated by APPNOTE.
        assert_eq!(decode_zip_text("日本語.txt".as_bytes(), true), "日本語.txt");
        // No EFS flag but valid UTF-8 (typical Unix-produced ZIP).
        assert_eq!(
            decode_zip_text("日本語.txt".as_bytes(), false),
            "日本語.txt"
        );
    }

    #[test]
    fn cp437_fallback_is_injective_and_replacement_free() {
        // Invalid as UTF-8 and as Shift_JIS (0x81/0x82 are SJIS lead
        // bytes, but 0x20 is not a valid trail byte).
        let x = [0x81, 0x20, 0x41];
        let y = [0x82, 0x20, 0x41];
        let dx = decode_zip_text(&x, false);
        let dy = decode_zip_text(&y, false);
        assert_eq!(dx, "ü A");
        assert_eq!(dy, "é A");
        assert_ne!(dx, dy);
        assert!(!dx.contains('\u{FFFD}'));
        assert!(!dy.contains('\u{FFFD}'));
    }

    #[test]
    fn efs_flag_with_invalid_utf8_falls_back_to_cp437_not_replacement() {
        // A lying EFS flag must not lose information either.
        let x = [0xFF, 0x41];
        let y = [0xFE, 0x41];
        let dx = decode_zip_text(&x, true);
        let dy = decode_zip_text(&y, true);
        assert_ne!(dx, dy);
        assert!(!dx.contains('\u{FFFD}'));
    }

    #[test]
    fn every_cp437_byte_decodes_uniquely() {
        let all: Vec<u8> = (0u8..=255).collect();
        let decoded = decode_cp437(&all);
        let chars: Vec<char> = decoded.chars().collect();
        assert_eq!(chars.len(), 256);
        for (i, a) in chars.iter().enumerate() {
            for b in chars.iter().skip(i + 1) {
                assert_ne!(a, b, "CP437 table has a duplicate mapping");
            }
        }
    }
}
