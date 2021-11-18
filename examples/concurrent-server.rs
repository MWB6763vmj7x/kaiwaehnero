#![feature(macro_rules, default_type_params)]

extern crate hyper;

use std::io::util::copy;
use std::io::net::ip::Ipv4Addr;
use std::sync::Arc;

use hyper::{Get, Post};
use hyper::server::{Server, Handler, Incoming, Request, Response};
use hyper::header::common::ContentLength;
use hyper::net::{HttpStream, HttpAcceptor, Fresh};

trait ConcurrentHandler: Send + Sync {
    fn handle(&self, req: Request, res: Response<Fresh>);
}

struct Concurrent<H: ConcurrentHandler> { handler: Arc<H> }

impl<H: ConcurrentHandler> Handler<HttpAcceptor, HttpStream> for Concurrent<H> {
    fn handle(self, mut incoming: Incoming) {
        for (mut req, mut res) in incoming {
            let clone = self.handler.clone();
            spawn(proc() { clone.handle(req, res) })
        }
    }
}

macro_rules! try_abort(
    ($e:expr) => {{
        match $e {
            Ok(v) => v,
            Err(..) => return
        }
    }}
)

struct Echo;

impl ConcurrentHandler for Echo {
    fn handle(&self, mut req: Request, mut res: Response<Fresh>) {
        match req.uri {
            hyper::uri::AbsolutePath(ref path) => match (&req.method, path.as_slice()) {
                (&Get, "/") | (&Get, "/echo") => {
                    let out = b"Try POST /echo";

                    res.headers_mut().set(ContentLength(out.len()));
                    let mut res = try_abort!(res.start());
                    try_abort!(res.write(out));
                    try_abort!(res.end());
                    return;
                },
                (&Post, "/echo") => (), // fall through, fighting mutable borrows
                _ => {
                    *res.status_mut() = hyper::status::NotFound;
                    try_abort!(res.start().and_then(|res| res.end()));
                    return;
                }
            },
            _ => {
                try_abort!(res.start().and_then(|res| res.end()));
                return;
            }
        }
        let mut res = try_abort!(res.start());
        try_abort!(copy(&mut req, &mut res));
        try_abort!(res.end());
    }
}

fn main() {
    let server = Server::http(Ipv4Addr(127, 0, 0, 1), 3000);
    server.listen(Concurrent { handler: Arc::new(Echo) }).unwrap();
}
