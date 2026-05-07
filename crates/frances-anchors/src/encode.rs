use crate::words::{BITS_PER_WORD, N_PADDING_WORDS, index_of, is_padding_marker, word};
use thiserror::Error;

const DATA_OFFSET: u16 = N_PADDING_WORDS as u16;
const DATA_MASK: u32 = (1 << BITS_PER_WORD) - 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("unknown word: {0}")]
    UnknownWord(String),
    #[error("padding marker appeared mid-stream")]
    PaddingInMiddle,
    #[error("malformed encoding (residual padding bits or invalid length)")]
    Malformed,
}

pub fn encode(bytes: &[u8]) -> Vec<u16> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let total_bits = bytes.len() as u32 * 8;
    let n_data = total_bits.div_ceil(BITS_PER_WORD);
    let pad_bits = (n_data * BITS_PER_WORD - total_bits) as u16;

    let mut out = Vec::with_capacity(n_data as usize + (pad_bits != 0) as usize);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buf = (buf << 8) | b as u32;
        bits += 8;
        if bits >= BITS_PER_WORD {
            bits -= BITS_PER_WORD;
            out.push(((buf >> bits) & DATA_MASK) as u16 + DATA_OFFSET);
            buf &= (1u32 << bits).wrapping_sub(1);
        }
    }
    if bits > 0 {
        out.push(((buf << (BITS_PER_WORD - bits)) & DATA_MASK) as u16 + DATA_OFFSET);
    }
    if pad_bits != 0 {
        out.push(pad_bits - 1);
    }
    out
}

pub fn decode(indices: &[u16]) -> Result<Vec<u8>, DecodeError> {
    let (data, pad_bits) = match indices.last() {
        Some(&last) if is_padding_marker(last) => {
            (&indices[..indices.len() - 1], (last + 1) as u32)
        }
        _ => (indices, 0u32),
    };
    for &idx in data {
        if is_padding_marker(idx) {
            return Err(DecodeError::PaddingInMiddle);
        }
    }
    let total_bits = data.len() as u32 * BITS_PER_WORD;
    if total_bits < pad_bits {
        return Err(DecodeError::Malformed);
    }
    let payload_bits = total_bits - pad_bits;
    if !payload_bits.is_multiple_of(8) {
        return Err(DecodeError::Malformed);
    }
    let byte_len = (payload_bits / 8) as usize;

    let mut out = Vec::with_capacity(byte_len);
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    for &idx in data {
        let value = (idx - DATA_OFFSET) as u64;
        buf = (buf << BITS_PER_WORD) | value;
        bits += BITS_PER_WORD;
        while bits >= 8 && out.len() < byte_len {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
            buf &= (1u64 << bits).wrapping_sub(1);
        }
    }
    if buf != 0 {
        return Err(DecodeError::Malformed);
    }
    Ok(out)
}

pub fn encode_words(bytes: &[u8]) -> Vec<&'static str> {
    encode(bytes).into_iter().map(word).collect()
}

pub fn decode_words(ws: &[&str]) -> Result<Vec<u8>, DecodeError> {
    let indices: Result<Vec<u16>, _> = ws
        .iter()
        .map(|w| index_of(w).ok_or_else(|| DecodeError::UnknownWord((*w).to_owned())))
        .collect();
    decode(&indices?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8]) {
        let encoded = encode(bytes);
        let decoded = decode(&encoded).expect("decode");
        assert_eq!(decoded, bytes, "round-trip failed for {bytes:?}");
    }

    #[test]
    fn empty_round_trip() {
        round_trip(&[]);
        assert_eq!(encode(&[]), Vec::<u16>::new());
    }

    #[test]
    fn all_residues_round_trip() {
        for len in 1..=26 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            round_trip(&bytes);
        }
    }

    #[test]
    fn hash_sized_payload() {
        let hash_bytes = [0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78];
        let encoded = encode(&hash_bytes);
        let total_bits = (hash_bytes.len() as u32) * 8;
        let n_data = total_bits.div_ceil(BITS_PER_WORD) as usize;
        let has_padding = !total_bits.is_multiple_of(BITS_PER_WORD);
        assert_eq!(encoded.len(), n_data + has_padding as usize);
        assert_eq!(is_padding_marker(*encoded.last().unwrap()), has_padding);
        round_trip(&hash_bytes);
    }

    /// `BITS_PER_WORD` bytes encode to exactly 8 data words with zero residual
    /// bits in the last word — so no padding marker is appended.
    #[test]
    fn payload_aligned_to_word_boundary_has_no_padding() {
        let bytes: Vec<u8> = (0..BITS_PER_WORD as u8).collect();
        let encoded = encode(&bytes);
        assert_eq!(encoded.len(), 8);
        assert!(!is_padding_marker(*encoded.last().unwrap()));
        round_trip(&bytes);
    }

    /// Every padding marker (indices 0..N_PADDING_WORDS) must be reachable by
    /// the encoder. If the dictionary shrinks below the number of distinct
    /// residual-bit counts, decoding would silently break for some lengths.
    #[test]
    fn every_padding_marker_is_emitted() {
        use std::collections::HashSet;
        // Byte lengths 1..=BITS_PER_WORD walk every residual modulo BITS_PER_WORD
        // (since gcd(8, BITS_PER_WORD) = 1 for any odd BITS_PER_WORD), which is
        // every non-zero pad-bit count from 1 to BITS_PER_WORD-1.
        let mut seen: HashSet<u16> = HashSet::new();
        for len in 1..=BITS_PER_WORD as usize {
            let bytes: Vec<u8> = (0..len as u8).collect();
            let last = *encode(&bytes).last().unwrap();
            if is_padding_marker(last) {
                seen.insert(last);
            }
        }
        let expected: HashSet<u16> = (0..N_PADDING_WORDS as u16).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn random_lengths_round_trip() {
        let mut state: u64 = 0x9E3779B97F4A7C15;
        for len in 0..256 {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (state >> 32) as u8
                })
                .collect();
            round_trip(&bytes);
        }
    }

    #[test]
    fn word_round_trip() {
        let bytes = b"hello, world!";
        let words = encode_words(bytes);
        let decoded = decode_words(&words).expect("decode");
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn unknown_word_errors() {
        let err = decode_words(&["NotARealWordInTheDictionary"]).unwrap_err();
        assert!(matches!(err, DecodeError::UnknownWord(_)));
    }

    #[test]
    fn padding_in_middle_errors() {
        let mut encoded = encode(b"hello");
        encoded.insert(1, 0);
        let err = decode(&encoded).unwrap_err();
        assert_eq!(err, DecodeError::PaddingInMiddle);
    }

    #[test]
    fn malformed_payload_bits() {
        let one_data = DATA_OFFSET;
        let marker0 = 0u16;
        let err = decode(&[one_data, marker0]).unwrap_err();
        assert_eq!(err, DecodeError::Malformed);
    }

    #[test]
    fn malformed_only_marker() {
        let err = decode(&[0u16]).unwrap_err();
        assert_eq!(err, DecodeError::Malformed);
    }
}
