use std::fmt;
use std::str::FromStr;

use frances_anchors::{
    DICT_SIZE, N_DATA_WORDS, N_PADDING_WORDS, index_of, is_padding_marker, word,
};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smallvec::SmallVec;
use thiserror::Error;

type Inner = SmallVec<[u16; 2]>;

const FIRST_DATA_IDX: u16 = N_PADDING_WORDS as u16;
const LAST_DATA_IDX: u16 = (N_PADDING_WORDS + N_DATA_WORDS - 1) as u16;

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct Anchor(Inner);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AnchorParseError {
    #[error("anchor word not found in dictionary: {0}")]
    UnknownWord(String),
    #[error("anchor word reserved as padding marker, not a data word: {0}")]
    PaddingMarker(String),
    #[error("anchor must be 1+ hyphen-separated words, no empty parts")]
    BadShape,
    #[error("anchor bytes must be a non-empty multiple of 2 (got {0})")]
    BadByteLength(usize),
    #[error("anchor index {0} is out of valid data-word range")]
    OutOfRange(u16),
}

impl Anchor {
    /// The smallest anchor in canonical iteration order.
    pub fn first() -> Self {
        let mut v = Inner::new();
        v.push(FIRST_DATA_IDX);
        Self(v)
    }

    /// Advance to the next anchor in canonical order. When all slots are at
    /// the maximum data index, extends to a longer anchor by resetting every
    /// slot to the first data index and pushing one more.
    pub fn increment(&mut self) {
        for i in (0..self.0.len()).rev() {
            if self.0[i] < LAST_DATA_IDX {
                self.0[i] += 1;
                for j in (i + 1)..self.0.len() {
                    self.0[j] = FIRST_DATA_IDX;
                }
                return;
            }
        }
        for d in self.0.iter_mut() {
            *d = FIRST_DATA_IDX;
        }
        self.0.push(FIRST_DATA_IDX);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|i| i.to_le_bytes()).collect()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AnchorParseError> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
            return Err(AnchorParseError::BadByteLength(bytes.len()));
        }
        let mut indices = Inner::new();
        for chunk in bytes.chunks_exact(2) {
            let idx = u16::from_le_bytes([chunk[0], chunk[1]]);
            if !is_valid_data_idx(idx) {
                return Err(AnchorParseError::OutOfRange(idx));
            }
            indices.push(idx);
        }
        Ok(Self(indices))
    }
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, idx) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            f.write_str(word(*idx))?;
        }
        Ok(())
    }
}

impl FromStr for Anchor {
    type Err = AnchorParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(AnchorParseError::BadShape);
        }
        let parts: Vec<&str> = s.split('-').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(AnchorParseError::BadShape);
        }
        let mut indices = Inner::new();
        for part in parts {
            indices.push(lookup_data_word(part)?);
        }
        Ok(Self(indices))
    }
}

impl Serialize for Anchor {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        if ser.is_human_readable() {
            ser.collect_str(self)
        } else {
            ser.serialize_bytes(&self.to_bytes())
        }
    }
}

impl<'de> Deserialize<'de> for Anchor {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct AnchorVisitor;

        impl<'de> Visitor<'de> for AnchorVisitor {
            type Value = Anchor;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anchor as hyphen-separated words or u16-LE bytes")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Anchor, E> {
                v.parse().map_err(de::Error::custom)
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Anchor, E> {
                Anchor::from_bytes(v).map_err(de::Error::custom)
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Anchor, E> {
                Anchor::from_bytes(&v).map_err(de::Error::custom)
            }
        }

        if de.is_human_readable() {
            de.deserialize_str(AnchorVisitor)
        } else {
            de.deserialize_bytes(AnchorVisitor)
        }
    }
}

fn is_valid_data_idx(idx: u16) -> bool {
    !is_padding_marker(idx) && (idx as usize) < DICT_SIZE
}

fn lookup_data_word(s: &str) -> Result<u16, AnchorParseError> {
    let idx = index_of(s).ok_or_else(|| AnchorParseError::UnknownWord(s.to_owned()))?;
    if is_padding_marker(idx) {
        return Err(AnchorParseError::PaddingMarker(s.to_owned()));
    }
    Ok(idx)
}

#[cfg(test)]
fn from_idxs(idxs: &[u16]) -> Anchor {
    let bytes: Vec<u8> = idxs.iter().flat_map(|i| i.to_le_bytes()).collect();
    Anchor::from_bytes(&bytes).expect("test indices must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_is_first_data_idx() {
        assert_eq!(Anchor::first(), from_idxs(&[FIRST_DATA_IDX]));
    }

    #[test]
    fn increment_within_single() {
        let mut a = Anchor::first();
        a.increment();
        assert_eq!(a, from_idxs(&[FIRST_DATA_IDX + 1]));
        a.increment();
        assert_eq!(a, from_idxs(&[FIRST_DATA_IDX + 2]));
    }

    #[test]
    fn increment_extends_at_overflow() {
        let mut a = from_idxs(&[LAST_DATA_IDX]);
        a.increment();
        assert_eq!(a, from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX]));
    }

    #[test]
    fn increment_within_compound() {
        let mut a = from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX]);
        a.increment();
        assert_eq!(a, from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX + 1]));
    }

    #[test]
    fn increment_carries_compound() {
        let mut a = from_idxs(&[FIRST_DATA_IDX, LAST_DATA_IDX]);
        a.increment();
        assert_eq!(a, from_idxs(&[FIRST_DATA_IDX + 1, FIRST_DATA_IDX]));
    }

    #[test]
    fn increment_extends_compound_at_overflow() {
        let mut a = from_idxs(&[LAST_DATA_IDX, LAST_DATA_IDX]);
        a.increment();
        assert_eq!(
            a,
            from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX, FIRST_DATA_IDX])
        );
    }

    #[test]
    fn display_one_and_many() {
        let a = from_idxs(&[FIRST_DATA_IDX]);
        let b = from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX + 1]);
        let c = from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX + 1, FIRST_DATA_IDX + 2]);
        assert!(!a.to_string().is_empty());
        assert_eq!(b.to_string().matches('-').count(), 1);
        assert_eq!(c.to_string().matches('-').count(), 2);
    }

    #[test]
    fn fromstr_round_trip() {
        let cases = [
            from_idxs(&[FIRST_DATA_IDX]),
            from_idxs(&[FIRST_DATA_IDX + 100]),
            from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX + 7]),
            from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX + 1, FIRST_DATA_IDX + 2]),
        ];
        for a in cases {
            let s = a.to_string();
            let parsed: Anchor = s.parse().expect("parse");
            assert_eq!(parsed, a);
        }
    }

    #[test]
    fn round_trip_bytes() {
        let cases = [
            from_idxs(&[FIRST_DATA_IDX]),
            from_idxs(&[FIRST_DATA_IDX + 5, FIRST_DATA_IDX + 6]),
            from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX + 1, FIRST_DATA_IDX + 2]),
        ];
        for a in cases {
            let bytes = a.to_bytes();
            let parsed = Anchor::from_bytes(&bytes).expect("from_bytes");
            assert_eq!(parsed, a);
        }
    }

    #[test]
    fn rejects_padding_marker_via_fromstr() {
        let pad_word = frances_anchors::word(0);
        let err: AnchorParseError = pad_word.parse::<Anchor>().unwrap_err();
        assert!(matches!(err, AnchorParseError::PaddingMarker(_)));
    }

    #[test]
    fn rejects_padding_marker_in_bytes() {
        let bytes = 0u16.to_le_bytes();
        assert_eq!(
            Anchor::from_bytes(&bytes),
            Err(AnchorParseError::OutOfRange(0))
        );
    }

    #[test]
    fn rejects_out_of_range_index() {
        let bytes = (DICT_SIZE as u16).to_le_bytes();
        assert!(matches!(
            Anchor::from_bytes(&bytes),
            Err(AnchorParseError::OutOfRange(_))
        ));
    }

    #[test]
    fn rejects_bad_shape() {
        assert_eq!("".parse::<Anchor>(), Err(AnchorParseError::BadShape));
        assert_eq!("-".parse::<Anchor>(), Err(AnchorParseError::BadShape));
        assert_eq!("a-".parse::<Anchor>(), Err(AnchorParseError::BadShape));
        assert_eq!("-a".parse::<Anchor>(), Err(AnchorParseError::BadShape));
    }

    #[test]
    fn rejects_unknown_word() {
        let err: AnchorParseError = "Xqzfooblahnotword".parse::<Anchor>().unwrap_err();
        assert!(matches!(err, AnchorParseError::UnknownWord(_)));
    }

    #[test]
    fn rejects_bad_byte_length() {
        assert_eq!(
            Anchor::from_bytes(&[1, 2, 3]),
            Err(AnchorParseError::BadByteLength(3))
        );
        assert_eq!(
            Anchor::from_bytes(&[]),
            Err(AnchorParseError::BadByteLength(0))
        );
    }

    #[test]
    fn serde_human_readable_is_string() {
        let a = from_idxs(&[FIRST_DATA_IDX, FIRST_DATA_IDX + 1]);
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, format!("\"{a}\""));
        let back: Anchor = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn serde_binary_is_bytes() {
        let a = from_idxs(&[FIRST_DATA_IDX + 3, FIRST_DATA_IDX + 4]);
        let bin = bincode::serde::encode_to_vec(&a, bincode::config::standard()).unwrap();
        let (back, _): (Anchor, _) =
            bincode::serde::decode_from_slice(&bin, bincode::config::standard()).unwrap();
        assert_eq!(back, a);
    }
}
