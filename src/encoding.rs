use std::fmt;

use encoding_rs::SHIFT_JIS;
use serde::{Deserialize, Serialize};

use crate::{BlitzError, Result};

pub const UTF8_CHECK_LIMIT: usize = 4_096;
pub const ENCODING_DETECTION_LIMIT: usize = 65_536;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
const UTF16_LE_BOM: &[u8] = b"\xFF\xFE";
const UTF16_BE_BOM: &[u8] = b"\xFE\xFF";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Ansi,
}

impl TextEncoding {
    pub fn label(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf8Bom => "UTF-8 with BOM",
            TextEncoding::Utf16Le => "UTF-16 LE",
            TextEncoding::Utf16Be => "UTF-16 BE",
            TextEncoding::Ansi => "ANSI",
        }
    }

    pub fn bom_len(self, bytes: &[u8]) -> usize {
        match self {
            TextEncoding::Utf8Bom if bytes.starts_with(UTF8_BOM) => UTF8_BOM.len(),
            TextEncoding::Utf16Le if bytes.starts_with(UTF16_LE_BOM) => UTF16_LE_BOM.len(),
            TextEncoding::Utf16Be if bytes.starts_with(UTF16_BE_BOM) => UTF16_BE_BOM.len(),
            _ => 0,
        }
    }
}

impl fmt::Display for TextEncoding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

pub fn detect_encoding(bytes: &[u8]) -> TextEncoding {
    if bytes.starts_with(UTF8_BOM) {
        return TextEncoding::Utf8Bom;
    }
    if bytes.starts_with(UTF16_LE_BOM) {
        return TextEncoding::Utf16Le;
    }
    if bytes.starts_with(UTF16_BE_BOM) {
        return TextEncoding::Utf16Be;
    }

    let detection_sample = &bytes[..bytes.len().min(ENCODING_DETECTION_LIMIT)];
    if looks_like_utf16(detection_sample, Endian::Little) {
        return TextEncoding::Utf16Le;
    }
    if looks_like_utf16(detection_sample, Endian::Big) {
        return TextEncoding::Utf16Be;
    }

    let utf8_sample = &bytes[..bytes.len().min(UTF8_CHECK_LIMIT)];
    if std::str::from_utf8(utf8_sample).is_ok() {
        return TextEncoding::Utf8;
    }

    TextEncoding::Ansi
}

pub fn decode_to_utf8(bytes: &[u8], encoding: TextEncoding) -> Result<String> {
    let body = &bytes[encoding.bom_len(bytes)..];
    match encoding {
        TextEncoding::Utf8 | TextEncoding::Utf8Bom => std::str::from_utf8(body)
            .map(str::to_owned)
            .map_err(|error| BlitzError::Encoding(format!("invalid UTF-8: {error}"))),
        TextEncoding::Utf16Le => decode_utf16(body, Endian::Little),
        TextEncoding::Utf16Be => decode_utf16(body, Endian::Big),
        TextEncoding::Ansi => {
            let (text, _, had_errors) = SHIFT_JIS.decode(body);
            if had_errors {
                Err(BlitzError::Encoding(
                    "ANSI/Shift_JIS decoding reported malformed bytes".to_owned(),
                ))
            } else {
                Ok(text.into_owned())
            }
        }
    }
}

pub fn encode_from_utf8(text: &str, encoding: TextEncoding) -> Result<Vec<u8>> {
    match encoding {
        TextEncoding::Utf8 => Ok(text.as_bytes().to_vec()),
        TextEncoding::Utf8Bom => {
            let mut bytes = Vec::with_capacity(text.len() + UTF8_BOM.len());
            bytes.extend_from_slice(UTF8_BOM);
            bytes.extend_from_slice(text.as_bytes());
            Ok(bytes)
        }
        TextEncoding::Utf16Le => Ok(encode_utf16(text, Endian::Little)),
        TextEncoding::Utf16Be => Ok(encode_utf16(text, Endian::Big)),
        TextEncoding::Ansi => {
            let (bytes, _, had_errors) = SHIFT_JIS.encode(text);
            if had_errors {
                Err(BlitzError::Encoding(
                    "text contains characters that cannot be encoded as ANSI/Shift_JIS".to_owned(),
                ))
            } else {
                Ok(bytes.into_owned())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

fn decode_utf16(bytes: &[u8], endian: Endian) -> Result<String> {
    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return Err(BlitzError::Encoding(
            "UTF-16 byte length must be even".to_owned(),
        ));
    }

    let code_units = chunks
        .map(|chunk| match endian {
            Endian::Little => u16::from_le_bytes([chunk[0], chunk[1]]),
            Endian::Big => u16::from_be_bytes([chunk[0], chunk[1]]),
        })
        .collect::<Vec<_>>();

    String::from_utf16(&code_units)
        .map_err(|error| BlitzError::Encoding(format!("invalid UTF-16: {error}")))
}

fn encode_utf16(text: &str, endian: Endian) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
    match endian {
        Endian::Little => bytes.extend_from_slice(UTF16_LE_BOM),
        Endian::Big => bytes.extend_from_slice(UTF16_BE_BOM),
    }

    for code_unit in text.encode_utf16() {
        match endian {
            Endian::Little => bytes.extend_from_slice(&code_unit.to_le_bytes()),
            Endian::Big => bytes.extend_from_slice(&code_unit.to_be_bytes()),
        }
    }

    bytes
}

fn looks_like_utf16(bytes: &[u8], endian: Endian) -> bool {
    if bytes.len() < 8 {
        return false;
    }

    let mut text_byte_zeroes = 0usize;
    let mut zero_byte_zeroes = 0usize;
    let mut pairs = 0usize;

    for chunk in bytes.chunks_exact(2) {
        pairs += 1;
        let (text_byte, zero_byte) = match endian {
            Endian::Little => (chunk[0], chunk[1]),
            Endian::Big => (chunk[1], chunk[0]),
        };
        text_byte_zeroes += usize::from(text_byte == 0);
        zero_byte_zeroes += usize::from(zero_byte == 0);
    }

    zero_byte_zeroes > pairs / 3 && text_byte_zeroes <= pairs / 20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_utf8_bom() {
        assert_eq!(detect_encoding(b"\xEF\xBB\xBFhello"), TextEncoding::Utf8Bom);
    }

    #[test]
    fn detects_utf16_without_bom() {
        let bytes = [b'a', 0, b'b', 0, b'c', 0, b'\n', 0];
        assert_eq!(detect_encoding(&bytes), TextEncoding::Utf16Le);
    }

    #[test]
    fn round_trips_shift_jis_japanese() {
        let encoded = encode_from_utf8("日本語", TextEncoding::Ansi).expect("encode");
        assert_eq!(detect_encoding(&encoded), TextEncoding::Ansi);
        assert_eq!(
            decode_to_utf8(&encoded, TextEncoding::Ansi).expect("decode"),
            "日本語"
        );
    }
}
