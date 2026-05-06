mod encode;
mod hash;
mod words;

pub use encode::{DecodeError, decode, decode_words, encode, encode_words};
pub use hash::{hash_line, hash_lines};
pub use words::{
    BITS_PER_WORD, DICT_SIZE, N_DATA_WORDS, N_PADDING_WORDS, WORD_TO_INDEX, WORDS, index_of,
    is_padding_marker, word,
};
