# hyper

[![Build Status](https://travis-ci.org/hyperium/hyper.svg?branch=master)](https://travis-ci.org/hyperium/hyper)

A Modern HTTP library for Rust.

[Documentation](http://hyperium.github.io/hyper)

## Overview

Hyper is a fast, modern HTTP implementation written in and for Rust. It
is a low-level typesafe abstraction over raw HTTP, providing an elegant
layer over "stringly-typed" HTTP.

Hyper offers both an HTTP/S client and HTTP server which can be used to drive
complex web applications written entirely in Rust.

The documentation is located at [http://hyperium.github.io/hyper](http://hyperium.github.io/hyper).

__WARNING: Hyper is still under active development. The API is still changing
in non-backwards-compatible ways without warning.__

## Example

Hello World Server:

```rust
fn hello(_: Request, res: Response<Fresh>) {
    *res.status_mut() = status::Ok;
    let mut res = res.start().unwrap();
    res.write(b"Hello World!");
    res.end().unwrap();
}

fn main() {
    let server = Server::http(Ipv4Addr(127, 0, 0, 1), 1337);
    server.listen(hello).unwrap();
}
```

Client:

```rust
fn main() {
    // Create a client.
    let mut client = Client::new();

    // Creating an outgoing request.
    let mut res = client.get("http://www.gooogle.com/")
        // set a header
        .header(Connection(vec![Close]))
        // let 'er go!
        .send();

    // Read the Response.
    let body = res.read_to_string().unwrap();

    println!("Response: {}", res);
}
```

## Scientific\* Benchmarks

[Client Bench:](./benches/client.rs)

```
running 3 tests
test bench_curl  ... bench:    400253 ns/iter (+/- 143539)
test bench_hyper ... bench:    181703 ns/iter (+/- 46529)

test result: ok. 0 passed; 0 failed; 0 ignored; 2 measured
```

[Mock Client Bench:](./benches/client_mock_tcp.rs)

```
running 3 tests
test bench_mock_curl  ... bench:     53987 ns/iter (+/- 1735)
test bench_mock_http  ... bench:     43569 ns/iter (+/- 1409)
test bench_mock_hyper ... bench:     20996 ns/iter (+/- 1742)

test result: ok. 0 passed; 0 failed; 0 ignored; 3 measured
```


[Server Bench:](./benches/server.rs)

```
running 2 tests
test bench_http  ... bench:    296539 ns/iter (+/- 58861)
test bench_hyper ... bench:    233069 ns/iter (+/- 90194)

test result: ok. 0 passed; 0 failed; 0 ignored; 2 measured
```

\* No science was harmed in the making of this benchmark.

## License

[MIT](./LICENSE)

