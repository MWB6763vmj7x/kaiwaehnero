#![deny(warnings)]
extern crate hyper;
extern crate futures;
extern crate pretty_env_logger;
//extern crate num_cpus;

use hyper::header::{ContentLength, ContentType};
use hyper::server::{Server, Service, Request, Response};

static PHRASE: &'static [u8] = b"Hello World!";

#[derive(Clone, Copy)]
struct Hello;

impl Service for Hello {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = ::futures::Finished<Response, hyper::Error>;
    fn call(&self, _req: Request) -> Self::Future {
        ::futures::finished(
            Response::new()
                .with_header(ContentLength(PHRASE.len() as u64))
                .with_header(ContentType::plaintext())
                .with_body(PHRASE)
        )
    }

}

fn main() {
    pretty_env_logger::init().unwrap();
    let addr = "127.0.0.1:3000".parse().unwrap();
    let _server = Server::standalone(|tokio| {
        Server::http(&addr, tokio)?
            .handle(|| Ok(Hello), tokio)
    }).unwrap();
    println!("Listening on http://{}", addr);
}
