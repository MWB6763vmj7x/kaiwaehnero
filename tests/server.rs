#![deny(warnings)]
extern crate hyper;
extern crate futures;
extern crate spmc;
extern crate pretty_env_logger;
extern crate tokio_core;
extern crate tokio_io;

use futures::{Future, Stream};
use futures::future::{self, FutureResult, Either};
use futures::sync::oneshot;

use tokio_core::net::TcpListener;
use tokio_core::reactor::{Core, Timeout};
use tokio_io::{AsyncRead, AsyncWrite};

use std::net::{TcpStream, SocketAddr};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hyper::server::{Http, Request, Response, Service, NewService};


#[test]
fn get_should_ignore_body() {
    let server = serve();

    let mut req = connect(server.addr());
    // Connection: close = don't try to parse the body as a new request
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
        I shouldn't be read.\r\n\
    ").unwrap();
    req.read(&mut [0; 256]).unwrap();

    assert_eq!(server.body(), b"");
}

#[test]
fn get_with_body() {
    let server = serve();
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Content-Length: 19\r\n\
        \r\n\
        I'm a good request.\r\n\
    ").unwrap();
    req.read(&mut [0; 256]).unwrap();

    // note: doesn't include trailing \r\n, cause Content-Length wasn't 21
    assert_eq!(server.body(), b"I'm a good request.");
}

#[test]
fn get_implicitly_empty() {
    // See https://github.com/hyperium/hyper/issues/1373
    let mut core = Core::new().unwrap();
    let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap(), &core.handle()).unwrap();
    let addr = listener.local_addr().unwrap();

    thread::spawn(move || {
        let mut tcp = connect(&addr);
        tcp.write_all(b"\
            GET / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            \r\n\
        ").unwrap();
    });

    let fut = listener.incoming()
        .into_future()
        .map_err(|_| unreachable!())
        .and_then(|(item, _incoming)| {
            let (socket, _) = item.unwrap();
            Http::<hyper::Chunk>::new().serve_connection(socket, GetImplicitlyEmpty)
        });

    core.run(fut).unwrap();

    struct GetImplicitlyEmpty;

    impl Service for GetImplicitlyEmpty {
        type Request = Request;
        type Response = Response;
        type Error = hyper::Error;
        type Future = FutureResult<Self::Response, Self::Error>;

        fn call(&self, req: Request) -> Self::Future {
            assert!(req.body_ref().is_none());
            future::ok(Response::new())
        }
    }
}

#[test]
fn get_fixed_response() {
    let foo_bar = b"foo bar baz";
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(foo_bar.len() as u64))
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").unwrap();
    let mut body = String::new();
    req.read_to_string(&mut body).unwrap();
    let n = body.find("\r\n\r\n").unwrap() + 4;

    assert_eq!(&body[n..], "foo bar baz");
}

#[test]
fn get_chunked_response() {
    let foo_bar = b"foo bar baz";
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::TransferEncoding::chunked())
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").unwrap();
    let mut body = String::new();
    req.read_to_string(&mut body).unwrap();
    let n = body.find("\r\n\r\n").unwrap() + 4;

    assert_eq!(&body[n..], "B\r\nfoo bar baz\r\n0\r\n\r\n");
}

#[test]
fn get_chunked_response_with_ka() {
    let foo_bar = b"foo bar baz";
    let foo_bar_chunk = b"\r\nfoo bar baz\r\n0\r\n\r\n";
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::TransferEncoding::chunked())
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: keep-alive\r\n\
        \r\n\
    ").expect("writing 1");

    let mut buf = [0; 1024 * 4];
    let mut ntotal = 0;
    loop {
        let n = req.read(&mut buf[ntotal..]).expect("reading 1");
        ntotal = ntotal + n;
        assert!(ntotal < buf.len());
        if &buf[ntotal - foo_bar_chunk.len()..ntotal] == foo_bar_chunk {
            break;
        }
    }


    // try again!

    let quux = b"zar quux";
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(quux.len() as u64))
        .body(quux);
    req.write_all(b"\
        GET /quux HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").expect("writing 2");

    let mut buf = [0; 1024 * 8];
    loop {
        let n = req.read(&mut buf[..]).expect("reading 2");
        assert!(n > 0, "n = {}", n);
        if n < buf.len() && n > 0  {
            if &buf[n - quux.len()..n] == quux {
                break;
            }
        }
    }
}

#[test]
fn post_with_chunked_body() {
    let server = serve();
    let mut req = connect(server.addr());
    req.write_all(b"\
        POST / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Transfer-Encoding: chunked\r\n\
        \r\n\
        1\r\n\
        q\r\n\
        2\r\n\
        we\r\n\
        2\r\n\
        rt\r\n\
        0\r\n\
        \r\n\
    ").unwrap();
    req.read(&mut [0; 256]).unwrap();

    assert_eq!(server.body(), b"qwert");
}

#[test]
fn empty_response_chunked() {
    let server = serve();

    server.reply()
        .status(hyper::Ok)
        .body("");

    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Content-Length: 0\r\n\
        Connection: close\r\n\
        \r\n\
    ").unwrap();


    let mut response = String::new();
    req.read_to_string(&mut response).unwrap();

    assert!(response.contains("Transfer-Encoding: chunked\r\n"));

    let mut lines = response.lines();
    assert_eq!(lines.next(), Some("HTTP/1.1 200 OK"));

    let mut lines = lines.skip_while(|line| !line.is_empty());
    assert_eq!(lines.next(), Some(""));
    // 0\r\n\r\n
    assert_eq!(lines.next(), Some("0"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), None);
}

#[test]
fn empty_response_chunked_without_body_should_set_content_length() {
    extern crate pretty_env_logger;
    let _ = pretty_env_logger::try_init();
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::TransferEncoding::chunked());
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").unwrap();

    let mut response = String::new();
    req.read_to_string(&mut response).unwrap();

    assert!(!response.contains("Transfer-Encoding: chunked\r\n"));
    assert!(response.contains("Content-Length: 0\r\n"));

    let mut lines = response.lines();
    assert_eq!(lines.next(), Some("HTTP/1.1 200 OK"));

    let mut lines = lines.skip_while(|line| !line.is_empty());
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), None);
}

#[test]
fn head_response_can_send_content_length() {
    extern crate pretty_env_logger;
    let _ = pretty_env_logger::try_init();
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(1024));
    let mut req = connect(server.addr());
    req.write_all(b"\
        HEAD / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").unwrap();

    let mut response = String::new();
    req.read_to_string(&mut response).unwrap();

    assert!(response.contains("Content-Length: 1024\r\n"));

    let mut lines = response.lines();
    assert_eq!(lines.next(), Some("HTTP/1.1 200 OK"));

    let mut lines = lines.skip_while(|line| !line.is_empty());
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), None);
}

#[test]
fn response_does_not_set_chunked_if_body_not_allowed() {
    extern crate pretty_env_logger;
    let _ = pretty_env_logger::try_init();
    let server = serve();
    server.reply()
        .status(hyper::StatusCode::NotModified)
        .header(hyper::header::TransferEncoding::chunked());
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").unwrap();

    let mut response = String::new();
    req.read_to_string(&mut response).unwrap();

    assert!(!response.contains("Transfer-Encoding"));

    let mut lines = response.lines();
    assert_eq!(lines.next(), Some("HTTP/1.1 304 Not Modified"));

    // no body or 0\r\n\r\n
    let mut lines = lines.skip_while(|line| !line.is_empty());
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), None);
}

#[test]
fn keep_alive() {
    let foo_bar = b"foo bar baz";
    let server = serve();
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(foo_bar.len() as u64))
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: keep-alive\r\n\
        \r\n\
    ").expect("writing 1");

    let mut buf = [0; 1024 * 8];
    loop {
        let n = req.read(&mut buf[..]).expect("reading 1");
        if n < buf.len() {
            if &buf[n - foo_bar.len()..n] == foo_bar {
                break;
            } else {
            }
        }
    }

    // try again!

    let quux = b"zar quux";
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(quux.len() as u64))
        .body(quux);
    req.write_all(b"\
        GET /quux HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").expect("writing 2");

    let mut buf = [0; 1024 * 8];
    loop {
        let n = req.read(&mut buf[..]).expect("reading 2");
        assert!(n > 0, "n = {}", n);
        if n < buf.len() {
            if &buf[n - quux.len()..n] == quux {
                break;
            }
        }
    }
}

#[test]
fn disable_keep_alive() {
    let foo_bar = b"foo bar baz";
    let server = serve_with_options(ServeOptions {
        keep_alive_disabled: true,
        .. Default::default()
    });
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(foo_bar.len() as u64))
        .body(foo_bar);
    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: keep-alive\r\n\
        \r\n\
    ").expect("writing 1");

    let mut buf = [0; 1024 * 8];
    loop {
        let n = req.read(&mut buf[..]).expect("reading 1");
        if n < buf.len() {
            if &buf[n - foo_bar.len()..n] == foo_bar {
                break;
            } else {
            }
        }
    }

    // try again!

    let quux = b"zar quux";
    server.reply()
        .status(hyper::Ok)
        .header(hyper::header::ContentLength(quux.len() as u64))
        .body(quux);

    // the write can possibly succeed, since it fills the kernel buffer on the first write
    let _ = req.write_all(b"\
        GET /quux HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ");

    let mut buf = [0; 1024 * 8];
    match req.read(&mut buf[..]) {
        // Ok(0) means EOF, so a proper shutdown
        // Err(_) could mean ConnReset or something, also fine
        Ok(0) |
        Err(_) => {}
        Ok(n) => {
            panic!("read {} bytes on a disabled keep-alive socket", n);
        }
    }
}

#[test]
fn expect_continue() {
    let server = serve();
    let mut req = connect(server.addr());
    server.reply().status(hyper::Ok);

    req.write_all(b"\
        POST /foo HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Expect: 100-continue\r\n\
        Content-Length: 5\r\n\
        Connection: Close\r\n\
        \r\n\
    ").expect("write 1");

    let msg = b"HTTP/1.1 100 Continue\r\n\r\n";
    let mut buf = vec![0; msg.len()];
    req.read_exact(&mut buf).expect("read 1");
    assert_eq!(buf, msg);

    let msg = b"hello";
    req.write_all(msg).expect("write 2");

    let mut body = String::new();
    req.read_to_string(&mut body).expect("read 2");

    let body = server.body();
    assert_eq!(body, msg);
}

#[test]
fn pipeline_disabled() {
    let server = serve();
    let mut req = connect(server.addr());
    server.reply().status(hyper::Ok);
    server.reply().status(hyper::Ok);

    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        \r\n\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        \r\n\
    ").expect("write 1");

    let mut buf = vec![0; 4096];
    let n = req.read(&mut buf).expect("read 1");
    assert_ne!(n, 0);
    // Woah there. What?
    //
    // This test is wishy-washy because of race conditions in access of the
    // socket. The test is still useful, since it allows for the responses
    // to be received in 2 reads. But it might sometimes come in 1 read.
    //
    // TODO: add in a delay to the `ServeReply` interface, to allow this
    // delay to prevent the 2 writes from happening before this test thread
    // can read from the socket.
    match req.read(&mut buf) {
        Ok(n) => {
            // won't be 0, because we didn't say to close, and so socket
            // will be open until `server` drops
            assert_ne!(n, 0);
        }
        Err(_) => (),
    }
}

#[test]
fn pipeline_enabled() {
    let server = serve_with_options(ServeOptions {
        pipeline: true,
        .. Default::default()
    });
    let mut req = connect(server.addr());
    server.reply().status(hyper::Ok);
    server.reply().status(hyper::Ok);

    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        \r\n\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        Connection: close\r\n\
        \r\n\
    ").expect("write 1");

    let mut buf = vec![0; 4096];
    let n = req.read(&mut buf).expect("read 1");
    assert_ne!(n, 0);
    // with pipeline enabled, both responses should have been in the first read
    // so a second read should be EOF
    let n = req.read(&mut buf).expect("read 2");
    assert_eq!(n, 0);
}

#[test]
fn disable_keep_alive_mid_request() {
    let mut core = Core::new().unwrap();
    let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap(), &core.handle()).unwrap();
    let addr = listener.local_addr().unwrap();

    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();

    let child = thread::spawn(move || {
        let mut req = connect(&addr);
        req.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        tx1.send(()).unwrap();
        rx2.wait().unwrap();
        req.write_all(b"Host: localhost\r\n\r\n").unwrap();
        let mut buf = vec![];
        req.read_to_end(&mut buf).unwrap();
    });

    let fut = listener.incoming()
        .into_future()
        .map_err(|_| unreachable!())
        .and_then(|(item, _incoming)| {
            let (socket, _) = item.unwrap();
            Http::<hyper::Chunk>::new().serve_connection(socket, HelloWorld)
                .select2(rx1)
                .then(|r| {
                    match r {
                        Ok(Either::A(_)) => panic!("expected rx first"),
                        Ok(Either::B(((), mut conn))) => {
                            conn.disable_keep_alive();
                            tx2.send(()).unwrap();
                            conn
                        }
                        Err(Either::A((e, _))) => panic!("unexpected error {}", e),
                        Err(Either::B((e, _))) => panic!("unexpected error {}", e),
                    }
                })
        });

    core.run(fut).unwrap();
    child.join().unwrap();
}

#[test]
fn disable_keep_alive_post_request() {
    let mut core = Core::new().unwrap();
    let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap(), &core.handle()).unwrap();
    let addr = listener.local_addr().unwrap();

    let (tx1, rx1) = oneshot::channel();

    let child = thread::spawn(move || {
        let mut req = connect(&addr);
        req.write_all(b"\
            GET / HTTP/1.1\r\n\
            Host: localhost\r\n\
            \r\n\
        ").unwrap();

        let mut buf = [0; 1024 * 8];
        loop {
            let n = req.read(&mut buf).expect("reading 1");
            if n < buf.len() {
                if &buf[n - HELLO.len()..n] == HELLO.as_bytes() {
                    break;
                }
            }
        }

        tx1.send(()).unwrap();

        let nread = req.read(&mut buf).expect("keep-alive reading");
        assert_eq!(nread, 0);
    });

    let dropped = Dropped::new();
    let dropped2 = dropped.clone();
    let fut = listener.incoming()
        .into_future()
        .map_err(|_| unreachable!())
        .and_then(|(item, _incoming)| {
            let (socket, _) = item.expect("accepted socket");
            let transport = DebugStream {
                stream: socket,
                _debug: dropped2,
            };
            Http::<hyper::Chunk>::new().serve_connection(transport, HelloWorld)
                .select2(rx1)
                .then(|r| {
                    match r {
                        Ok(Either::A(_)) => panic!("expected rx first"),
                        Ok(Either::B(((), mut conn))) => {
                            conn.disable_keep_alive();
                            conn
                        }
                        Err(Either::A((e, _))) => panic!("unexpected error {}", e),
                        Err(Either::B((e, _))) => panic!("unexpected error {}", e),
                    }
                })
        });

    assert!(!dropped.load());
    core.run(fut).unwrap();
    // we must poll the Core one more time in order for Windows to drop
    // the read-blocked socket.
    //
    // See https://github.com/carllerche/mio/issues/776
    let timeout = Timeout::new(Duration::from_millis(10), &core.handle()).unwrap();
    core.run(timeout).unwrap();
    assert!(dropped.load());
    child.join().unwrap();
}

#[test]
fn empty_parse_eof_does_not_return_error() {
    let mut core = Core::new().unwrap();
    let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap(), &core.handle()).unwrap();
    let addr = listener.local_addr().unwrap();

    thread::spawn(move || {
        let _tcp = connect(&addr);
    });

    let fut = listener.incoming()
        .into_future()
        .map_err(|_| unreachable!())
        .and_then(|(item, _incoming)| {
            let (socket, _) = item.unwrap();
            Http::<hyper::Chunk>::new().serve_connection(socket, HelloWorld)
        });

    core.run(fut).unwrap();
}

#[test]
fn nonempty_parse_eof_returns_error() {
    let mut core = Core::new().unwrap();
    let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap(), &core.handle()).unwrap();
    let addr = listener.local_addr().unwrap();

    thread::spawn(move || {
        let mut tcp = connect(&addr);
        tcp.write_all(b"GET / HTTP/1.1").unwrap();
    });

    let fut = listener.incoming()
        .into_future()
        .map_err(|_| unreachable!())
        .and_then(|(item, _incoming)| {
            let (socket, _) = item.unwrap();
            Http::<hyper::Chunk>::new().serve_connection(socket, HelloWorld)
                .map(|_| ())
        });

    core.run(fut).unwrap_err();
}

#[test]
fn remote_addr() {
    let server = serve();

    let mut req = connect(server.addr());
    req.write_all(b"\
        GET / HTTP/1.1\r\n\
        Host: example.domain\r\n\
        \r\n\
    ").unwrap();
    req.read(&mut [0; 256]).unwrap();

    let client_addr = req.local_addr().unwrap();
    assert_eq!(server.remote_addr(), client_addr);
}

// -------------------------------------------------
// the Server that is used to run all the tests with
// -------------------------------------------------

struct Serve {
    addr: SocketAddr,
    msg_rx: mpsc::Receiver<Msg>,
    reply_tx: spmc::Sender<Reply>,
    shutdown_signal: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Serve {
    fn addr(&self) -> &SocketAddr {
        &self.addr
    }

    pub fn remote_addr(&self) -> SocketAddr {
        match self.msg_rx.try_recv() {
            Ok(Msg::Addr(addr)) => addr,
            other => panic!("expected remote addr, found: {:?}", other),
        }
    }

    fn body(&self) -> Vec<u8> {
        let mut buf = vec![];
        loop {
            match self.msg_rx.try_recv() {
                Ok(Msg::Chunk(msg)) => {
                    buf.extend(&msg);
                },
                Ok(Msg::Addr(_)) => {},
                Err(_) => break,
            }
        }
        buf
    }

    fn reply(&self) -> ReplyBuilder {
        ReplyBuilder {
            tx: &self.reply_tx
        }
    }
}

struct ReplyBuilder<'a> {
    tx: &'a spmc::Sender<Reply>,
}

impl<'a> ReplyBuilder<'a> {
    fn status(self, status: hyper::StatusCode) -> Self {
        self.tx.send(Reply::Status(status)).unwrap();
        self
    }

    fn header<H: hyper::header::Header>(self, header: H) -> Self {
        let mut headers = hyper::Headers::new();
        headers.set(header);
        self.tx.send(Reply::Headers(headers)).unwrap();
        self
    }

    fn body<T: AsRef<[u8]>>(self, body: T) {
        self.tx.send(Reply::Body(body.as_ref().into())).unwrap();
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        drop(self.shutdown_signal.take());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[derive(Clone)]
struct TestService {
    tx: Arc<Mutex<mpsc::Sender<Msg>>>,
    reply: spmc::Receiver<Reply>,
    _timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
enum Reply {
    Status(hyper::StatusCode),
    Headers(hyper::Headers),
    Body(Vec<u8>),
}

#[derive(Debug)]
enum Msg {
    //Head(Request),
    Addr(SocketAddr),
    Chunk(Vec<u8>),
}

impl NewService for TestService {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;

    type Instance = TestService;

    fn new_service(&self) -> std::io::Result<TestService> {
        Ok(self.clone())
    }
}

impl Service for TestService {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item=Response, Error=hyper::Error>>;
    fn call(&self, req: Request) -> Self::Future {
        let tx = self.tx.clone();

        #[allow(deprecated)]
        let remote_addr = req.remote_addr().expect("remote_addr");
        tx.lock().unwrap().send(Msg::Addr(remote_addr)).unwrap();

        let replies = self.reply.clone();
        Box::new(req.body().for_each(move |chunk| {
            tx.lock().unwrap().send(Msg::Chunk(chunk.to_vec())).unwrap();
            Ok(())
        }).map(move |_| {
            let mut res = Response::new();
            while let Ok(reply) = replies.try_recv() {
                match reply {
                    Reply::Status(s) => {
                        res.set_status(s);
                    },
                    Reply::Headers(headers) => {
                        *res.headers_mut() = headers;
                    },
                    Reply::Body(body) => {
                        res.set_body(body);
                    },
                }
            }
            res
        }))
    }

}

const HELLO: &'static str = "hello";

struct HelloWorld;

impl Service for HelloWorld {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = FutureResult<Self::Response, Self::Error>;

    fn call(&self, _req: Request) -> Self::Future {
        let mut response = Response::new();
        response.headers_mut().set(hyper::header::ContentLength(HELLO.len() as u64));
        response.set_body(HELLO);
        future::ok(response)
    }
}


fn connect(addr: &SocketAddr) -> TcpStream {
    let req = TcpStream::connect(addr).unwrap();
    req.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    req.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
    req
}

fn serve() -> Serve {
    serve_with_options(Default::default())
}

struct ServeOptions {
    keep_alive_disabled: bool,
    pipeline: bool,
    timeout: Option<Duration>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            keep_alive_disabled: false,
            pipeline: false,
            timeout: None,
        }
    }
}

fn serve_with_options(options: ServeOptions) -> Serve {
    let _ = pretty_env_logger::try_init();

    let (addr_tx, addr_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = spmc::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let addr = "127.0.0.1:0".parse().unwrap();

    let keep_alive = !options.keep_alive_disabled;
    let pipeline = options.pipeline;
    let dur = options.timeout;

    let thread_name = format!("test-server-{:?}", dur);
    let thread = thread::Builder::new().name(thread_name).spawn(move || {
        let srv = Http::new()
            .keep_alive(keep_alive)
            .pipeline(pipeline)
            .bind(&addr, TestService {
                tx: Arc::new(Mutex::new(msg_tx.clone())),
                _timeout: dur,
                reply: reply_rx,
            }).unwrap();
        addr_tx.send(srv.local_addr().unwrap()).unwrap();
        srv.run_until(shutdown_rx.then(|_| Ok(()))).unwrap();
    }).unwrap();

    let addr = addr_rx.recv().unwrap();

    Serve {
        msg_rx: msg_rx,
        reply_tx: reply_tx,
        addr: addr,
        shutdown_signal: Some(shutdown_tx),
        thread: Some(thread),
    }
}

struct DebugStream<T, D> {
    stream: T,
    _debug: D,
}

impl<T: Read, D> Read for DebugStream<T, D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<T: Write, D> Write for DebugStream<T, D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}


impl<T: AsyncWrite, D> AsyncWrite for DebugStream<T, D> {
    fn shutdown(&mut self) -> futures::Poll<(), io::Error> {
        self.stream.shutdown()
    }
}


impl<T: AsyncRead, D> AsyncRead for DebugStream<T, D> {}

#[derive(Clone)]
struct Dropped(Arc<AtomicBool>);

impl Dropped {
    pub fn new() -> Dropped {
        Dropped(Arc::new(AtomicBool::new(false)))
    }

    pub fn load(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
