use header::QualityItem;
use std::str::FromStr;
use std::fmt;

/// A language tag.
/// See http://www.w3.org/Protocols/rfc2616/rfc2616-sec3.html#sec3.10
#[derive(Clone, PartialEq, Debug)]
pub struct Language{
    primary: String,
    sub: Option<String>
}

impl FromStr for Language {
    type Err = ();
    fn from_str(s: &str) -> Result<Language, ()> {
        let mut i = s.split("-");
        let p = i.next();
        let s = i.next();
        match (p, s) {
            (Some(p),Some(s)) => Ok(Language{primary: p.to_string(),
                                             sub: Some(s.to_string())}),
            (Some(p),_) => Ok(Language{primary: p.to_string(), sub: None}),
            _ => Err(())
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "{}", self.primary));
        match self.sub {
            Some(ref s) => write!(f, "-{}", s),
            None => Ok(())
        }
    }
}

header! {
    #[doc="`Accept-Language` header, defined in"]
    #[doc="[RFC7231](http://tools.ietf.org/html/rfc7231#section-5.3.5)"]
    #[doc=""]
    #[doc="The `Accept-Language` header field can be used by user agents to"]
    #[doc="indicate the set of natural languages that are preferred in the"]
    #[doc="response."]
    #[doc=""]
    #[doc="# ABNF"]
    #[doc="```plain"]
    #[doc="Accept-Language = 1#( language-range [ weight ] )"]
    #[doc="language-range  = <language-range, see [RFC4647], Section 2.1>"]
    #[doc="```"]
    (AcceptLanguage, "Accept-Language") => (QualityItem<Language>)+
}

#[cfg(test)]
mod tests {
    use header::{Header, qitem, Quality, QualityItem};
    use super::*;

    #[test]
    fn test_parse_header() {
        let a: AcceptLanguage = Header::parse_header(
            [b"en-us;q=1.0, en;q=0.5, fr".to_vec()].as_ref()).unwrap();
        let b = AcceptLanguage(vec![
            qitem(Language{primary: "en".to_string(), sub: Some("us".to_string())}),
            QualityItem::new(Language{primary: "en".to_string(), sub: None},
                             Quality(500)),
            qitem(Language{primary: "fr".to_string(), sub: None}),
        ]);
        assert_eq!(format!("{}", a), format!("{}", b));
        assert_eq!(a, b);
    }

    #[test]
    fn test_display() {
        assert_eq!("en".to_string(),
                   format!("{}", Language{primary: "en".to_string(),
                                          sub: None}));
        assert_eq!("en-us".to_string(),
                   format!("{}", Language{primary: "en".to_string(),
                                          sub: Some("us".to_string())}));
    }

    #[test]
    fn test_from_str() {
        assert_eq!(Language { primary: "en".to_string(), sub: None },
                   "en".parse().unwrap());
        assert_eq!(Language { primary: "en".to_string(),
                              sub: Some("us".to_string()) },
                   "en-us".parse().unwrap());
    }
}

bench_header!(bench, AcceptLanguage,
              { vec![b"en-us;q=1.0, en;q=0.5, fr".to_vec()] });
