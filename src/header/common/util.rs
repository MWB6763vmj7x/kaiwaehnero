//! Utility functions for Header implementations.

use std::str::{FromStr, from_utf8};
use std::fmt::{mod, Show};
use time::{Tm, strptime};

/// Reads a single raw string when parsing a header
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

/// Reads a comma-delimited raw header into a Vec.
#[inline]
pub fn from_comma_delimited<T: FromStr>(raw: &[Vec<u8>]) -> Option<Vec<T>> {
    if raw.len() != 1 {
        return None;
    }
    // we JUST checked that raw.len() == 1, so raw[0] WILL exist.
    from_one_comma_delimited(unsafe { raw.as_slice().unsafe_get(0).as_slice() })
}

/// Reads a comma-delimited raw string into a Vec.
pub fn from_one_comma_delimited<T: FromStr>(raw: &[u8]) -> Option<Vec<T>> {
    match from_utf8(raw) {
        Some(s) => {
            Some(s.as_slice()
                 .split([',', ' '].as_slice())
                 .filter_map(from_str)
                 .collect())
        }
        None => None
    }
}

/// Format an array into a comma-delimited string.
pub fn fmt_comma_delimited<T: Show>(fmt: &mut fmt::Formatter, parts: &[T]) -> fmt::Result {
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        try!(part.fmt(fmt));
        if i < last {
            try!(", ".fmt(fmt));
        }
    }
    Ok(())
}

/// Get a Tm from HTTP date formats.
//    Prior to 1995, there were three different formats commonly used by
//   servers to communicate timestamps.  For compatibility with old
//   implementations, all three are defined here.  The preferred format is
//   a fixed-length and single-zone subset of the date and time
//   specification used by the Internet Message Format [RFC5322].
//
//     HTTP-date    = IMF-fixdate / obs-date
//
//   An example of the preferred format is
//
//     Sun, 06 Nov 1994 08:49:37 GMT    ; IMF-fixdate
//
//   Examples of the two obsolete formats are
//
//     Sunday, 06-Nov-94 08:49:37 GMT   ; obsolete RFC 850 format
//     Sun Nov  6 08:49:37 1994         ; ANSI C's asctime() format
//
//   A recipient that parses a timestamp value in an HTTP header field
//   MUST accept all three HTTP-date formats.  When a sender generates a
//   header field that contains one or more timestamps defined as
//   HTTP-date, the sender MUST generate those timestamps in the
//   IMF-fixdate format.
pub fn tm_from_str(s: &str) -> Option<Tm> {
    strptime(s, "%a, %d %b %Y %T %Z").or_else(|_| {
        strptime(s, "%A, %d-%b-%y %T %Z")
    }).or_else(|_| {
        strptime(s, "%c")
    }).ok()
}
