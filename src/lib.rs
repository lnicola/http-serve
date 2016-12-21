// The MIT License (MIT)
// Copyright (c) 2016 Scott Lamb <slamb@slamb.org>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

#[cfg(test)] #[macro_use] extern crate lazy_static;
#[macro_use] extern crate log;
extern crate hyper;
#[macro_use] extern crate mime;
extern crate smallvec;

use hyper::server::{Request, Response};
use hyper::header;
use hyper::method::Method;
use hyper::net::Fresh;
use smallvec::SmallVec;
use std::cmp;
use std::io;
use std::ops::Range;

/// An HTTP entity for GET and HEAD serving.
pub trait Entity<Error> where Error: From<io::Error> {
    /// Returns the length of the slice in bytes.
    fn len(&self) -> u64;

    /// Writes bytes within this slice indicated by `range` to `out.`
    fn write_to(&self, range: Range<u64>, out: &mut io::Write) -> Result<(), Error>;

    fn content_type(&self) -> mime::Mime;
    fn etag(&self) -> Option<&header::EntityTag>;
    fn last_modified(&self) -> &header::HttpDate;
}

#[derive(Debug, Eq, PartialEq)]
enum ResolvedRanges {
    None,
    NotSatisfiable,
    Satisfiable(SmallVec<[Range<u64>; 1]>)
}

fn parse_range_header(range: Option<&header::Range>, resource_len: u64) -> ResolvedRanges {
    if let Some(&header::Range::Bytes(ref byte_ranges)) = range {
        let mut ranges: SmallVec<[Range<u64>; 1]> = SmallVec::new();
        for range in byte_ranges {
            match *range {
                header::ByteRangeSpec::FromTo(range_from, range_to) => {
                    let end = cmp::min(range_to + 1, resource_len);
                    if range_from >= end {
                        continue;
                    }
                    ranges.push(Range{start: range_from, end: end});
                },
                header::ByteRangeSpec::AllFrom(range_from) => {
                    if range_from >= resource_len {
                        continue;
                    }
                    ranges.push(Range{start: range_from, end: resource_len});
                },
                header::ByteRangeSpec::Last(last) => {
                    if last >= resource_len {
                        continue;
                    }
                    ranges.push(Range{start: resource_len - last,
                                      end: resource_len});
                },
            }
        }
        if !ranges.is_empty() {
            return ResolvedRanges::Satisfiable(ranges);
        }
        return ResolvedRanges::NotSatisfiable;
    }
    ResolvedRanges::None
}

/// Returns true if `req` doesn't have an `If-None-Match` header matching `req`.
fn none_match(etag: Option<&header::EntityTag>, req: &Request) -> bool {
    match req.headers.get::<header::IfNoneMatch>() {
        Some(&header::IfNoneMatch::Any) => false,
        Some(&header::IfNoneMatch::Items(ref items)) => {
            if let Some(some_etag) = etag {
                for item in items {
                    if item.weak_eq(some_etag) {
                        return false;
                    }
                }
            }
            true
        },
        None => true,
    }
}

/// Returns true if `req` has no `If-Match` header or one which matches `etag`.
fn any_match(etag: Option<&header::EntityTag>, req: &Request) -> bool {
    match req.headers.get::<header::IfMatch>() {
        // The absent header and "If-Match: *" cases differ only when there is no entity to serve.
        // We always have an entity to serve, so consider them identical.
        None | Some(&header::IfMatch::Any) => true,
        Some(&header::IfMatch::Items(ref items)) => {
            if let Some(some_etag) = etag {
                for item in items {
                    if item.strong_eq(some_etag) {
                        return true;
                    }
                }
            }
            false
        },
    }
}

/// Serves GET and HEAD requests for a given byte-ranged resource.
/// Handles conditional & subrange requests.
/// The caller is expected to have already determined the correct resource and appended
/// Expires, Cache-Control, and Vary headers.
///
/// TODO: is it appropriate to include those headers on all response codes used in this function?
///
/// TODO: check HTTP rules about weak vs strong comparisons with range requests. I don't think I'm
/// doing this correctly.
pub fn serve<Error>(e: &Entity<Error>, req: &Request, mut res: Response<Fresh>)
                    -> Result<(), Error> where Error: From<io::Error> {
    if req.method != Method::Get && req.method != Method::Head {
        *res.status_mut() = hyper::status::StatusCode::MethodNotAllowed;
        res.headers_mut().set(header::ContentType(mime!(Text/Plain)));
        res.headers_mut().set(header::Allow(vec![Method::Get, Method::Head]));
        res.send(b"This resource only supports GET and HEAD.")?;
        return Ok(());
    }

    let last_modified = e.last_modified();
    let etag = e.etag();
    res.headers_mut().set(header::AcceptRanges(vec![header::RangeUnit::Bytes]));
    res.headers_mut().set(header::LastModified(*last_modified));
    if let Some(some_etag) = etag {
        res.headers_mut().set(header::ETag(some_etag.clone()));
    }

    if let Some(&header::IfUnmodifiedSince(ref since)) = req.headers.get() {
        if last_modified.0.to_timespec() > since.0.to_timespec() {
            *res.status_mut() = hyper::status::StatusCode::PreconditionFailed;
            res.send(b"Precondition failed")?;
            return Ok(());
        }
    }

    if !any_match(etag, req) {
        *res.status_mut() = hyper::status::StatusCode::PreconditionFailed;
        res.send(b"Precondition failed")?;
        return Ok(());
    }

    if !none_match(etag, req) {
        *res.status_mut() = hyper::status::StatusCode::NotModified;
        res.send(b"")?;
        return Ok(());
    }

    if let Some(&header::IfModifiedSince(ref since)) = req.headers.get() {
        if last_modified <= since {
            *res.status_mut() = hyper::status::StatusCode::NotModified;
            res.send(b"")?;
            return Ok(());
        }
    }

    let mut range_hdr = req.headers.get::<header::Range>();

    // See RFC 2616 section 10.2.7: a Partial Content response should include certain
    // entity-headers or not based on the If-Range response.
    let include_entity_headers_on_range = match req.headers.get::<header::IfRange>() {
        Some(&header::IfRange::EntityTag(ref if_etag)) => {
            if let Some(some_etag) = etag {
                if if_etag.strong_eq(some_etag) {
                    false
                } else {
                    range_hdr = None;
                    true
                }
            } else {
                range_hdr = None;
                true
            }
        },
        Some(&header::IfRange::Date(ref if_date)) => {
            // The to_timespec conversion appears necessary because in the If-Range off the wire,
            // fields such as tm_yday are absent, causing strict equality to spuriously fail.
            if if_date.0.to_timespec() != last_modified.0.to_timespec() {
                range_hdr = None;
                true
            } else {
                false
            }
        },
        None => true,
    };
    let len = e.len();
    let (range, include_entity_headers) = match parse_range_header(range_hdr, len) {
        ResolvedRanges::None => (0 .. len, true),
        ResolvedRanges::Satisfiable(rs) => {
            if rs.len() == 1 {
                res.headers_mut().set(header::ContentRange(
                    header::ContentRangeSpec::Bytes{
                        range: Some((rs[0].start, rs[0].end-1)),
                        instance_length: Some(len)}));
                *res.status_mut() = hyper::status::StatusCode::PartialContent;
                (rs[0].clone(), include_entity_headers_on_range)
            } else {
                // Ignore multi-part range headers for now. They require additional complexity, and
                // I don't see clients sending them in the wild.
                (0 .. len, true)
            }
        },
        ResolvedRanges::NotSatisfiable => {
            res.headers_mut().set(header::ContentRange(
                header::ContentRangeSpec::Bytes{
                    range: None,
                    instance_length: Some(len)}));
            *res.status_mut() = hyper::status::StatusCode::RangeNotSatisfiable;
            res.send(b"")?;
            return Ok(());
        }
    };
    if include_entity_headers {
        res.headers_mut().set(header::ContentType(e.content_type()));
    }
    res.headers_mut().set(header::ContentLength(range.end - range.start));
    let mut stream = res.start()?;
    if req.method == Method::Get {
        e.write_to(range, &mut stream)?;
    }
    stream.end()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate env_logger;

    use hyper;
    use hyper::header::{self, ByteRangeSpec, ContentRangeSpec, EntityTag};
    use hyper::header::Range::Bytes;
    use mime;
    use smallvec::SmallVec;
    use std::io::{self, Read, Write};
    use std::ops::Range;
    use super::{ResolvedRanges, parse_range_header};
    use super::*;

    /// Tests the specific examples enumerated in RFC 2616 section 14.35.1.
    #[test]
    fn test_resolve_ranges_rfc() {
        let mut v = SmallVec::new();

        v.push(0 .. 500);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 499)])),
                                      10000));

        v.clear();
        v.push(500 .. 1000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(500, 999)])),
                                      10000));

        v.clear();
        v.push(9500 .. 10000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::Last(500)])),
                                      10000));

        v.clear();
        v.push(9500 .. 10000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::AllFrom(9500)])),
                                      10000));

        v.clear();
        v.push(0 .. 1);
        v.push(9999 .. 10000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 0),
                                                              ByteRangeSpec::Last(1)])),
                                      10000));

        // Non-canonical ranges. Possibly the point of these is that the adjacent and overlapping
        // ranges are supposed to be coalesced into one? I'm not going to do that for now.

        v.clear();
        v.push(500 .. 601);
        v.push(601 .. 1000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(500, 600),
                                                              ByteRangeSpec::FromTo(601, 999)])),
                                      10000));

        v.clear();
        v.push(500 .. 701);
        v.push(601 .. 1000);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(500, 700),
                                                              ByteRangeSpec::FromTo(601, 999)])),
                                      10000));
    }

    #[test]
    fn test_resolve_ranges_satisfiability() {
        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::AllFrom(10000)])),
                                      10000));

        let mut v = SmallVec::new();
        v.push(0 .. 500);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 499),
                                                              ByteRangeSpec::AllFrom(10000)])),
                                      10000));

        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::Last(1)])), 0));
        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 0)])), 0));
        assert_eq!(ResolvedRanges::NotSatisfiable,
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::AllFrom(0)])), 0));

        v.clear();
        v.push(0 .. 1);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 0)])), 1));

        v.clear();
        v.push(0 .. 500);
        assert_eq!(ResolvedRanges::Satisfiable(v.clone()),
                   parse_range_header(Some(&Bytes(vec![ByteRangeSpec::FromTo(0, 10000)])),
                                      500));
    }

    #[test]
    fn test_resolve_ranges_absent_or_invalid() {
        assert_eq!(ResolvedRanges::None, parse_range_header(None, 10000));
    }

    struct FakeEntity {
        etag: Option<EntityTag>,
        mime: mime::Mime,
        last_modified: header::HttpDate,
        body: &'static [u8],
    }

    impl Entity<io::Error> for FakeEntity {
        fn len(&self) -> u64 { self.body.len() as u64 }
        fn write_to(&self, range: Range<u64>, out: &mut Write) -> Result<(), io::Error> {
            out.write_all(&self.body[range.start as usize .. range.end as usize])
        }
        fn content_type(&self) -> mime::Mime { self.mime.clone() }
        fn etag(&self) -> Option<&EntityTag> { self.etag.as_ref() }
        fn last_modified(&self) -> &header::HttpDate { &self.last_modified }
    }

    fn new_server() -> String {
        let mut listener = hyper::net::HttpListener::new("127.0.0.1:0").unwrap();
        use hyper::net::NetworkListener;
        let addr = listener.local_addr().unwrap();
        let server = hyper::Server::new(listener);
        use std::thread::spawn;
        spawn(move || {
            use hyper::server::{Request, Response, Fresh};
            let _ = server.handle(move |req: Request, res: Response<Fresh>| {
                use hyper::uri::RequestUri;
                let path = match req.uri {
                    RequestUri::AbsolutePath(ref p) => p,
                    x => panic!("unexpected uri type {:?}", x),
                };
                let entity = match path.as_str() {
                    "/none" => &*ENTITY_NO_ETAG,
                    "/strong" => &*ENTITY_STRONG_ETAG,
                    "/weak" => &*ENTITY_WEAK_ETAG,
                    p => panic!("unexpected path {}", p),
                };
                serve(entity, &req, res).unwrap();
            });
        });
        format!("http://{}:{}", addr.ip(), addr.port())
    }

    lazy_static! {
        static ref SOME_DATE: header::HttpDate = {
            "Sun, 06 Nov 1994 08:49:37 GMT".parse::<header::HttpDate>().unwrap()
        };
        static ref LATER_DATE: header::HttpDate = {
            "Sun, 06 Nov 1994 09:49:37 GMT".parse::<header::HttpDate>().unwrap()
        };
        static ref ENTITY_NO_ETAG: FakeEntity = FakeEntity{
            etag: None,
            mime: mime!(Application/OctetStream),
            last_modified: *SOME_DATE,
            body: b"01234",
        };
        static ref ENTITY_STRONG_ETAG: FakeEntity = FakeEntity{
            etag: Some(EntityTag::strong("foo".to_owned())),
            mime: mime!(Application/OctetStream),
            last_modified: *SOME_DATE,
            body: b"01234",
        };
        static ref ENTITY_WEAK_ETAG: FakeEntity = FakeEntity{
            etag: Some(EntityTag::strong("foo".to_owned())),
            mime: mime!(Application/OctetStream),
            last_modified: *SOME_DATE,
            body: b"01234",
        };
        static ref SERVER: String = { new_server() };
    }

    #[test]
    fn serve_without_etag() {
        let _ = env_logger::init();
        let client = hyper::Client::new();
        let mut buf = Vec::new();
        let url = format!("{}/none", *SERVER);

        // Full body.
        let mut resp = client.get(&url).send().unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // If-Match any should still send the full body.
        let mut resp = client.get(&url)
                             .header(header::IfMatch::Any)
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // If-Match by etag doesn't match (as this request has no etag).
        let resp =
            client.get(&url)
                  .header(header::IfMatch::Items(vec![EntityTag::strong("foo".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::PreconditionFailed, resp.status);

        // If-None-Match any.
        let mut resp = client.get(&url)
                             .header(header::IfNoneMatch::Any)
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::NotModified, resp.status);
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"", &buf[..]);

        // If-None-Match by etag doesn't match (as this request has no etag).
        let mut resp =
            client.get(&url)
                  .header(header::IfNoneMatch::Items(vec![EntityTag::strong("foo".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // Unmodified since supplied date.
        let mut resp = client.get(&url)
                             .header(header::IfModifiedSince(*SOME_DATE))
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::NotModified, resp.status);
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"", &buf[..]);

        // Range serving - basic case.
        let mut resp = client.get(&url)
                             .header(Bytes(vec![ByteRangeSpec::FromTo(1, 3)]))
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::PartialContent, resp.status);
        assert_eq!(Some(&header::ContentRange(ContentRangeSpec::Bytes{
            range: Some((1, 3)),
            instance_length: Some(5),
        })), resp.headers.get());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"123", &buf[..]);

        // Range serving - multiple ranges. Currently falls back to whole range.
        let mut resp = client.get(&url)
                             .header(Bytes(vec![ByteRangeSpec::FromTo(0, 1),
                                                ByteRangeSpec::FromTo(3, 4)]))
                             .send()
                             .unwrap();
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // Range serving - not satisfiable.
        let mut resp = client.get(&url)
                             .header(Bytes(vec![ByteRangeSpec::AllFrom(500)]))
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::RangeNotSatisfiable, resp.status);
        assert_eq!(Some(&header::ContentRange(ContentRangeSpec::Bytes{
            range: None,
            instance_length: Some(5),
        })), resp.headers.get());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"", &buf[..]);

        // Range serving - matching If-Range by date honors the range.
        let mut resp = client.get(&url)
                             .header(Bytes(vec![ByteRangeSpec::FromTo(1, 3)]))
                             .header(header::IfRange::Date(*SOME_DATE))
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::PartialContent, resp.status);
        assert_eq!(Some(&header::ContentRange(ContentRangeSpec::Bytes{
            range: Some((1, 3)),
            instance_length: Some(5),
        })), resp.headers.get());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"123", &buf[..]);

        // Range serving - non-matching If-Range by date ignores the range.
        let mut resp = client.get(&url)
                             .header(Bytes(vec![ByteRangeSpec::FromTo(1, 3)]))
                             .header(header::IfRange::Date(*LATER_DATE))
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // Range serving - this resource has no etag, so any If-Range by etag ignores the range.
        let mut resp =
            client.get(&url)
                  .header(Bytes(vec![ByteRangeSpec::FromTo(1, 3)]))
                  .header(header::IfRange::EntityTag(EntityTag::strong("foo".to_owned())))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);
    }

    #[test]
    fn serve_with_strong_etag() {
        let _ = env_logger::init();
        let client = hyper::Client::new();
        let mut buf = Vec::new();
        let url = format!("{}/strong", *SERVER);

        // If-Match any should still send the full body.
        let mut resp = client.get(&url)
                             .header(header::IfMatch::Any)
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // If-Match by matching etag should send the full body.
        let mut resp =
            client.get(&url)
                  .header(header::IfMatch::Items(vec![EntityTag::strong("foo".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // If-Match by etag which doesn't match.
        let resp =
            client.get(&url)
                  .header(header::IfMatch::Items(vec![EntityTag::strong("bar".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::PreconditionFailed, resp.status);

        // If-None-Match by etag which matches.
        let mut resp =
            client.get(&url)
                  .header(header::IfNoneMatch::Items(vec![EntityTag::strong("foo".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::NotModified, resp.status);
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"", &buf[..]);

        // If-None-Match by etag which doesn't match.
        let mut resp =
            client.get(&url)
                  .header(header::IfNoneMatch::Items(vec![EntityTag::strong("bar".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // Range serving - If-Range matching by etag.
        let mut resp =
            client.get(&url)
                  .header(Bytes(vec![ByteRangeSpec::FromTo(1, 3)]))
                  .header(header::IfRange::EntityTag(EntityTag::strong("foo".to_owned())))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::PartialContent, resp.status);
        assert_eq!(None, resp.headers.get::<header::ContentType>());
        assert_eq!(Some(&header::ContentRange(ContentRangeSpec::Bytes{
            range: Some((1, 3)),
            instance_length: Some(5),
        })), resp.headers.get());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"123", &buf[..]);

        // Range serving - If-Range not matching by etag.
        let mut resp =
            client.get(&url)
                  .header(Bytes(vec![ByteRangeSpec::FromTo(1, 3)]))
                  .header(header::IfRange::EntityTag(EntityTag::strong("bar".to_owned())))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);
    }

    #[test]
    fn serve_with_weak_etag() {
        let _ = env_logger::init();
        let client = hyper::Client::new();
        let mut buf = Vec::new();
        let url = format!("{}/weak", *SERVER);

        // If-Match any should still send the full body.
        let mut resp = client.get(&url)
                             .header(header::IfMatch::Any)
                             .send()
                             .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // If-Match by etag doesn't match because matches use the strong comparison function.
        let resp =
            client.get(&url)
                  .header(header::IfMatch::Items(vec![EntityTag::weak("foo".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::PreconditionFailed, resp.status);

        // If-None-Match by identical weak etag is sufficient.
        let mut resp =
            client.get(&url)
                  .header(header::IfNoneMatch::Items(vec![EntityTag::weak("foo".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::NotModified, resp.status);
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"", &buf[..]);

        // If-None-Match by etag which doesn't match.
        let mut resp =
            client.get(&url)
                  .header(header::IfNoneMatch::Items(vec![EntityTag::weak("bar".to_owned())]))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);

        // Range serving - If-Range matching by weak etag isn't sufficient.
        let mut resp =
            client.get(&url)
                  .header(Bytes(vec![ByteRangeSpec::FromTo(1, 3)]))
                  .header(header::IfRange::EntityTag(EntityTag::weak("foo".to_owned())))
                  .send()
                  .unwrap();
        assert_eq!(hyper::status::StatusCode::Ok, resp.status);
        assert_eq!(Some(&header::ContentType(mime!(Application/OctetStream))),
                   resp.headers.get::<header::ContentType>());
        assert_eq!(None, resp.headers.get::<header::ContentRange>());
        buf.clear();
        resp.read_to_end(&mut buf).unwrap();
        assert_eq!(b"01234", &buf[..]);
    }
}
