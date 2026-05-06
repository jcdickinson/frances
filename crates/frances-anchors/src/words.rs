use std::collections::HashMap;
use std::sync::LazyLock;

static WORDS_TXT: &str = include_str!("../words.txt");

pub const BITS_PER_WORD: u32 = 13;
pub const N_PADDING_WORDS: usize = 12;
pub const N_DATA_WORDS: usize = 1 << BITS_PER_WORD;
pub const DICT_SIZE: usize = N_PADDING_WORDS + N_DATA_WORDS;

pub static WORDS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let v: Vec<&'static str> = WORDS_TXT.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        v.len(),
        DICT_SIZE,
        "word list must be exactly {DICT_SIZE} entries"
    );
    v
});

pub static WORD_TO_INDEX: LazyLock<HashMap<&'static str, u16>> = LazyLock::new(|| {
    WORDS
        .iter()
        .enumerate()
        .map(|(i, w)| (*w, i as u16))
        .collect()
});

pub fn word(idx: u16) -> &'static str {
    WORDS[idx as usize]
}

pub fn index_of(w: &str) -> Option<u16> {
    WORD_TO_INDEX.get(w).copied()
}

pub fn is_padding_marker(idx: u16) -> bool {
    (idx as usize) < N_PADDING_WORDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_size() {
        assert_eq!(WORDS.len(), DICT_SIZE);
        assert_eq!(WORDS.len(), 8204);
    }

    #[test]
    fn no_duplicates() {
        assert_eq!(WORDS.len(), WORD_TO_INDEX.len());
    }

    #[test]
    fn round_trip_all_indices() {
        for (i, w) in WORDS.iter().enumerate() {
            let idx = i as u16;
            assert_eq!(index_of(w), Some(idx));
            assert_eq!(word(idx), *w);
        }
    }

    #[test]
    fn padding_marker_range() {
        for i in 0..N_PADDING_WORDS {
            assert!(is_padding_marker(i as u16));
        }
        for i in N_PADDING_WORDS..DICT_SIZE {
            assert!(!is_padding_marker(i as u16));
        }
    }
}
