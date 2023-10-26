#![deny(warnings)]
extern crate hyper;

extern crate env_logger;

use std::env;

use hyper::Client;

fn main() {
    env_logger::init().unwrap();

    let url = match env::args().nth(1) {
        Some(url) => url,
        None => {
            println!("Usage: client <url>");
            return;
        }
    };

    let mut client = Client::new();

    let res = match client.get(&*url).send() {
        Ok(res) => res,
        Err(err) => panic!("Failed to connect: {:?}", err)
    };

    println!("Response: {}", res.status);
    println!("Headers:\n{}", res.headers);
    //TODO: add copy back when std::stdio impls std::io::Write.
}
