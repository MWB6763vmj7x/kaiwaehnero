use header::{Header, HeaderFormat};
use std::fmt;
use std::str::FromStr;
use header::parsing::{from_comma_delimited, fmt_comma_delimited};

use self::Protocol::{WebSocket, ProtocolExt};

/// The `Upgrade` header.
#[derive(Clone, PartialEq, Show)]
pub struct Upgrade(pub Vec<Protocol>);

deref!(Upgrade => Vec<Protocol>);

/// Protocol values that can appear in the Upgrade header.
#[derive(Clone, PartialEq, Show)]
pub enum Protocol {
    /// The websocket protocol.
    WebSocket,
    /// Some other less common protocol.
    ProtocolExt(String),
}

impl FromStr for Protocol {
    fn from_str(s: &str) -> Option<Protocol> {
        match s {
            "websocket" => Some(WebSocket),
            s => Some(ProtocolExt(s.to_string()))
        }
    }
}

impl fmt::String for Protocol {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}", match *self {
            WebSocket => "websocket",
            ProtocolExt(ref s) => s.as_slice()
        })
    }
}

impl Header for Upgrade {
    fn header_name(_: Option<Upgrade>) -> &'static str {
        "Upgrade"
    }

    fn parse_header(raw: &[Vec<u8>]) -> Option<Upgrade> {
        from_comma_delimited(raw).map(|vec| Upgrade(vec))
    }
}

impl HeaderFormat for Upgrade {
    fn fmt_header(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let Upgrade(ref parts) = *self;
        fmt_comma_delimited(fmt, &parts[])
    }
}

bench_header!(bench, Upgrade, { vec![b"HTTP/2.0, RTA/x11, websocket".to_vec()] });

