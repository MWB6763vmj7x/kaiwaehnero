//! Utility functions for Header implementations.

use std::str::{FromStr, from_utf8};

/// Utility function that reads a single raw string when parsing a header
pub fn from_one_raw_str<T: FromStr>(raw: &[Vec<u8>]) -> Option<T> {
    if raw.len() != 1 {
        return None;
    }
    // we JUST checked that raw.len() == 1, so raw[0] WILL exist.
    match from_utf8(unsafe { raw[].unsafe_get(0)[] }) {
        Some(s) => FromStr::from_str(s),
        None => None
    }
}
