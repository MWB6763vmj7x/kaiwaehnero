#![doc(html_root_url = "https://hyperium.github.io/hyper/")]
#![deny(missing_docs)]
#![deny(warnings)]
#![deny(missing_debug_implementations)]
#![cfg_attr(all(test, feature = "nightly"), feature(test))]

//! # Hyper
//!
//! Hyper is a fast, modern HTTP implementation written in and for Rust. It
//! is a low-level typesafe abstraction over raw HTTP, providing an elegant
//! layer over "stringly-typed" HTTP.
//!
//! Hyper provides both a [Client](client/index.html) and a
//! [Server](server/index.html), along with a
//! [typed Headers system](header/index.html).

extern crate bytes;
#[macro_use] extern crate futures;
extern crate futures_cpupool;
extern crate httparse;
extern crate language_tags;
#[macro_use] extern crate log;
pub extern crate mime;
extern crate base64;
extern crate time;
extern crate tokio_core as tokio;
#[macro_use] extern crate tokio_io;
extern crate tokio_proto;
extern crate tokio_service;
extern crate unicase;
#[macro_use] extern crate url;

#[cfg(all(test, feature = "nightly"))]
extern crate test;


pub use uri::Uri;
pub use client::Client;
pub use error::{Result, Error};
pub use header::Headers;
pub use http::{Body, Chunk};
pub use http::request::Request;
pub use http::response::Response;
pub use method::Method::{self, Get, Head, Post, Put, Delete};
pub use status::StatusCode::{self, Ok, BadRequest, NotFound};
pub use server::Server;
pub use version::HttpVersion;

#[cfg(test)]
mod mock;
pub mod client;
pub mod error;
mod method;
pub mod header;
mod http;
pub mod server;
pub mod status;
mod uri;
mod version;
