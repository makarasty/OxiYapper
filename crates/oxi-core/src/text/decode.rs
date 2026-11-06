use thiserror::Error;

/// A user text file (`dictionary.txt`, `replace.txt`, hallucination lines) that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("file is not valid UTF-8")]
    InvalidUtf8,
    #[error("file is not valid UTF-16")]
    InvalidUtf16,
}

/// Decode a text file saved by an editor: UTF-8 with or without BOM, or UTF-16 LE/BE with BOM
/// (Notepad). Anything else that is not valid UTF-8 is an error.
pub fn decode_text_file(bytes: &[u8]) -> Result<String, DecodeError> {
    let utf16 = |rest: &[u8], from: fn([u8; 2]) -> u16| {
        let (pairs, odd) = rest.as_chunks::<2>();
        if !odd.is_empty() {
            return Err(DecodeError::InvalidUtf16);
        }
        let units: Vec<u16> = pairs.iter().map(|&p| from(p)).collect();
        String::from_utf16(&units).map_err(|_| DecodeError::InvalidUtf16)
    };
    let utf8 = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| DecodeError::InvalidUtf8);
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => utf8(rest),
        _ => utf8(bytes),
    }
}
