// Copyright (c) 2016-2018 Scott Lamb <slamb@slamb.org>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![feature(test)]

extern crate futures;
extern crate futures_cpupool;
extern crate http;
extern crate http_serve;
extern crate hyper;
#[macro_use]
extern crate lazy_static;
extern crate mime;
extern crate reqwest;
extern crate tempdir;
extern crate test;
extern crate tokio;

use futures::Future;
use futures_cpupool::{CpuFuture, CpuPool};
use http::{Request, Response};
use hyper::Body;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Mutex;
use tempdir::TempDir;

fn serve(req: Request<Body>) -> CpuFuture<Response<Body>, ::std::io::Error> {
    POOL.spawn_fn(move || {
        let f = File::open(&*PATH.lock().unwrap())?;
        let headers = http::header::HeaderMap::new();
        let f = http_serve::ChunkedReadFile::new(f, Some(POOL.clone()), headers)?;
        Ok(http_serve::serve(f, &req))
    })
}

/// Returns the hostport of a newly created, never-destructed server.
fn new_server() -> String {
    let (tx, rx) = ::std::sync::mpsc::channel();
    ::std::thread::spawn(move || {
        let addr = "127.0.0.1:0".parse().unwrap();
        let server = hyper::server::Server::bind(&addr).serve(|| hyper::service::service_fn(serve));
        let addr = server.local_addr();
        tx.send(addr).unwrap();
        tokio::run(server.map_err(|e| panic!(e)))
    });
    let addr = rx.recv().unwrap();
    format!("http://{}:{}", addr.ip(), addr.port())
}

lazy_static! {
    static ref PATH: Mutex<OsString> = { Mutex::new(OsString::new()) };
    static ref POOL: CpuPool = { CpuPool::new(1) };
    static ref SERVER: String = { new_server() };
}

/// Sets up the server to serve a 1 MiB file, until the returned `TempDir` goes out of scope and the
/// file is deleted.
fn setup(kib: usize) -> TempDir {
    let tmpdir = tempdir::TempDir::new("http-file-bench").unwrap();
    let tmppath = tmpdir.path().join("f");
    {
        let p = &mut *PATH.lock().unwrap();
        p.clear();
        p.push(&tmppath);
    }
    let mut tmpfile = File::create(tmppath).unwrap();
    for _ in 0..kib {
        tmpfile.write_all(&[0; 1024]).unwrap();
    }
    tmpdir
}

fn serve_full_entity(b: &mut test::Bencher, kib: usize) {
    let _tmpdir = setup(kib);
    let client = reqwest::Client::new();
    let mut buf = Vec::with_capacity(kib);
    let mut run = || {
        let mut resp = client.get(&*SERVER).send().unwrap();
        buf.clear();
        let size = resp.read_to_end(&mut buf).unwrap();
        assert_eq!(reqwest::StatusCode::Ok, resp.status());
        assert_eq!(1024 * kib, size);
    };
    run(); // warm.
    b.iter(run);
    b.bytes = 1024 * kib as u64;
}

#[bench]
fn serve_full_entity_1mib(b: &mut test::Bencher) {
    serve_full_entity(b, 1024);
}

#[bench]
fn serve_full_entity_1kib(b: &mut test::Bencher) {
    serve_full_entity(b, 1);
}

#[bench]
fn serve_last_byte_1mib(b: &mut test::Bencher) {
    let _tmpdir = setup(1024);
    let client = reqwest::Client::new();
    let mut buf = Vec::with_capacity(1);
    let mut run = || {
        let mut resp = client
            .get(&*SERVER)
            .header(reqwest::header::Range::Bytes(vec![
                reqwest::header::ByteRangeSpec::Last(1),
            ]))
            .send()
            .unwrap();
        buf.clear();
        let size = resp.read_to_end(&mut buf).unwrap();
        assert_eq!(reqwest::StatusCode::PartialContent, resp.status());
        assert_eq!(1, size);
    };
    run(); // warm.
    b.iter(run);
}
