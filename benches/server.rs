#![feature(test)]
#![deny(warnings)]

extern crate futures;
extern crate hyper;
extern crate test;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;

use futures::{future, stream, Future};
use futures::sync::oneshot;

use hyper::header::{ContentLength, ContentType, TransferEncoding};
use hyper::server::{self, Service};

macro_rules! bench_server {
    ($b:ident, $header:expr, $body:expr) => ({
        let (_until_tx, until_rx) = oneshot::channel();
        let addr = {
            let (addr_tx, addr_rx) = mpsc::channel();
            ::std::thread::spawn(move || {
                let addr = "127.0.0.1:0".parse().unwrap();
                let srv = hyper::server::Http::new().bind(&addr, || Ok(BenchPayload {
                    header: $header,
                    body: $body,
                })).unwrap();
                let addr = srv.local_addr().unwrap();
                addr_tx.send(addr).unwrap();
                srv.run_until(until_rx.map_err(|_| ())).unwrap();
            });

            addr_rx.recv().unwrap()
        };

        let total_bytes = {
            let mut tcp = TcpStream::connect(addr).unwrap();
            tcp.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
            let mut buf = Vec::new();
            tcp.read_to_end(&mut buf).unwrap()
        };

        let mut tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(::std::time::Duration::from_secs(3))).unwrap();
        let mut buf = [0u8; 8192];

        $b.bytes = 35 + total_bytes as u64;
        $b.iter(|| {
            tcp.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut sum = 0;
            while sum < total_bytes {
                sum += tcp.read(&mut buf).unwrap();
            }
            assert_eq!(sum, total_bytes);
        })
    })
}

fn body(b: &'static [u8]) -> hyper::Body {
    b.into()
}

#[bench]
fn throughput_fixedsize_small_payload(b: &mut test::Bencher) {
    bench_server!(b, ContentLength(13), || body(b"Hello, World!"))
}

#[bench]
fn throughput_fixedsize_large_payload(b: &mut test::Bencher) {
    bench_server!(b, ContentLength(1_000_000), ||  body(&[b'x'; 1_000_000]))
}

#[bench]
fn throughput_fixedsize_many_chunks(b: &mut test::Bencher) {
    bench_server!(b, ContentLength(1_000_000), || {
        static S: &'static [&'static [u8]] = &[&[b'x'; 1_000] as &[u8]; 1_000] as _;
        stream::iter_ok(S.iter())
    })
}

#[bench]
fn throughput_chunked_small_payload(b: &mut test::Bencher) {
    bench_server!(b, TransferEncoding::chunked(), || body(b"Hello, World!"))
}

#[bench]
fn throughput_chunked_large_payload(b: &mut test::Bencher) {
    bench_server!(b, TransferEncoding::chunked(), ||  body(&[b'x'; 1_000_000]))
}

#[bench]
fn throughput_chunked_many_chunks(b: &mut test::Bencher) {
    bench_server!(b, TransferEncoding::chunked(), || {
        static S: &'static [&'static [u8]] = &[&[b'x'; 1_000] as &[u8]; 1_000] as _;
        stream::iter_ok(S.iter())
    })
}

#[bench]
fn raw_tcp_throughput_small_payload(b: &mut test::Bencher) {
    let (tx, rx) = mpsc::channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    ::std::thread::spawn(move || {
        let mut sock = listener.accept().unwrap().0;

        let mut buf = [0u8; 8192];
        while rx.try_recv().is_err() {
            sock.read(&mut buf).unwrap();
            sock.write_all(b"\
                HTTP/1.1 200 OK\r\n\
                Content-Length: 13\r\n\
                Content-Type: text/plain; charset=utf-8\r\n\
                Date: Fri, 12 May 2017 18:21:45 GMT\r\n\
                \r\n\
                Hello, World!\
            ").unwrap();
        }
    });

    let mut tcp = TcpStream::connect(addr).unwrap();
    let mut buf = [0u8; 4096];

    b.bytes = 130 + 35;
    b.iter(|| {
        tcp.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let n = tcp.read(&mut buf).unwrap();
        assert_eq!(n, 130);
    });
    tx.send(()).unwrap();
}

#[bench]
fn raw_tcp_throughput_large_payload(b: &mut test::Bencher) {
    let (tx, rx) = mpsc::channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let srv_head = b"\
        HTTP/1.1 200 OK\r\n\
        Content-Length: 1000000\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        Date: Fri, 12 May 2017 18:21:45 GMT\r\n\
        \r\n\
    ";
    ::std::thread::spawn(move || {
        let mut sock = listener.accept().unwrap().0;

        let mut buf = [0u8; 8192];
        while rx.try_recv().is_err() {
            let r = sock.read(&mut buf).unwrap();
            if r == 0 {
                break;
            }
            sock.write_all(srv_head).unwrap();
            sock.write_all(&[b'x'; 1_000_000]).unwrap();
        }
    });

    let mut tcp = TcpStream::connect(addr).unwrap();
    let mut buf = [0u8; 8192];

    let expect_read = srv_head.len() + 1_000_000;
    b.bytes = expect_read as u64 + 35;

    b.iter(|| {
        tcp.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut sum = 0;
        while sum < expect_read {
            sum += tcp.read(&mut buf).unwrap();
        }
        assert_eq!(sum, expect_read);
    });
    tx.send(()).unwrap();
}

struct BenchPayload<H, F> {
    header: H,
    body: F,
}

impl<H, F, B> Service for BenchPayload<H, F>
where
    H: hyper::header::Header + Clone,
    F: Fn() -> B,
{
    type Request = server::Request;
    type Response = server::Response<B>;
    type Error = hyper::Error;
    type Future = future::FutureResult<Self::Response, hyper::Error>;
    fn call(&self, _req: Self::Request) -> Self::Future {
        future::ok(
            server::Response::new()
                .with_header(self.header.clone())
                .with_header(ContentType::plaintext())
                .with_body((self.body)())
        )
    }
}
