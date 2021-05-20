#![feature(macro_rules)]

extern crate hyper;
extern crate debug;

use std::io::util::copy;
use std::io::net::ip::Ipv4Addr;

use hyper::{Get, Post};
use hyper::server::{Server, Handler, Incoming};
use hyper::header::common::ContentLength;

struct Echo;

macro_rules! try_continue(
    ($e:expr) => {{
        match $e {
            Ok(v) => v,
            Err(e) => { println!("Error: {}", e); continue; }
        }
    }}
)

impl Handler for Echo {
    fn handle(self, mut incoming: Incoming) {
        for (mut req, mut res) in incoming {
            match req.uri {
                hyper::uri::AbsolutePath(ref path) => match (&req.method, path.as_slice()) {
                    (&Get, "/") | (&Get, "/echo") => {
                        let out = b"Try POST /echo";

                        res.headers.set(ContentLength(out.len()));
                        try_continue!(res.write(out));
                        try_continue!(res.end());
                        continue;
                    },
                    (&Post, "/echo") => (), // fall through, fighting mutable borrows
                    _ => {
                        res.status = hyper::status::NotFound;
                        try_continue!(res.end());
                        continue;
                    }
                },
                _ => {
                    try_continue!(res.end());
                    continue; 
                }
            };

            try_continue!(copy(&mut req, &mut res));
            try_continue!(res.end());
        }
    }
}

fn main() {
    let server = Server::http(Ipv4Addr(127, 0, 0, 1), 1337);
    server.listen(Echo).unwrap();
}
