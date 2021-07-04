use anyhow::bail;
use bytes::{Bytes, BytesMut};
use futures::future::Either;
use futures::{future, ready, stream, FutureExt, Stream, StreamExt, TryFutureExt};
use headers::{
    AcceptRanges, ContentLength, ContentRange, ContentType, HeaderMapExt, IfModifiedSince, IfRange,
    IfUnmodifiedSince, LastModified, Range,
};
use hyper::{Body, Request, Response, StatusCode};
use percent_encoding::percent_decode_str;
use routerify::prelude::*;
use std::pin::Pin;
use std::task::Poll;
use tokio::fs::File;
use tokio::io::AsyncSeekExt;
use tokio_util::codec::{BytesCodec, FramedRead};
use tokio_util::io::poll_read_buf;

use crate::types::{
    AppState, AppUuid, BrowseResponse, DidlLite, Envelope, ListItemWrapper, XMLBody,
    CONTENT_DIRECTORY_XMLNS, ENVELOPE_ENCODING_STYLE, ENVELOPE_XMLNS, ENVELOPE_XMLNS_S, XMLNS_DC,
    XMLNS_DIDL, XMLNS_UPNP,
};
use crate::utils::read_directory;

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const ROOT_XML: &str = include_str!("root.xml");
const CONTENT_DESC_XML: &str = include_str!("content_desc.xml");
const CONNECTION_DESC_XML: &str = include_str!("connection_desc.xml");
const SOAP_ACTION: &str = "Soapaction";

fn get_browse_response(list_items: &Vec<ListItemWrapper>) -> String {
    let didl_result = DidlLite {
        xmlns_dc: XMLNS_DC.to_string(),
        xmlns_upnp: XMLNS_UPNP.to_string(),
        xmlns: XMLNS_DIDL.to_string(),
        list_items: list_items.iter().map(|x| x.list_item.clone()).collect(),
    };
    let response = Envelope {
        encoding_style: ENVELOPE_ENCODING_STYLE.to_string(),
        xmlns: ENVELOPE_XMLNS.to_string(),
        xmlns_s: ENVELOPE_XMLNS_S.to_string(),
        body: XMLBody {
            browse_response: BrowseResponse {
                xmlns_u: CONTENT_DIRECTORY_XMLNS.to_string(),
                number_returned: 1,
                total_matches: 1,
                update_id: 1,
                result: "{didl-result}".to_string(),
            },
        },
    };
    use strong_xml::XmlWrite;

    let didl_result = didl_result
        .to_string()
        .unwrap()
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");

    response
        .to_string()
        .unwrap()
        .replace("{didl-result}", &didl_result)
}

pub async fn root_handler(req: Request<Body>) -> anyhow::Result<Response<Body>> {
    let app_uuid = req.data::<AppUuid>().unwrap();
    let uuid_string = app_uuid.as_ref().to_string();
    let body = ROOT_XML
        .replace("{name}", "Actix-Rednithin-Dev")
        .replace("{uuid}", &uuid_string.clone());

    Ok(Response::builder()
        .header("Content-Type", "text/xml")
        .body(Body::from(body))
        .unwrap())
}

pub async fn content_desc_handler(_req: Request<Body>) -> anyhow::Result<Response<Body>> {
    Ok(Response::builder()
        .header("Content-Type", "text/xml")
        .body(Body::from(CONTENT_DESC_XML))
        .unwrap())
}

pub async fn content_handler(req: Request<Body>) -> anyhow::Result<Response<Body>> {
    let hostname = req
        .headers()
        .get(hyper::header::HOST)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let soap_action_header = req.headers().get(SOAP_ACTION).unwrap().to_str().unwrap();

    let action = match soap_action_header
        .trim_matches('"')
        .split("#")
        .collect::<Vec<&str>>()
        .get(1)
    {
        Some(&x) => x,
        _ => "",
    };
    let app_state = req.data::<Arc<Mutex<AppState>>>().unwrap().clone();
    let body_vec = hyper::body::to_bytes(req).await.unwrap();
    let body_string = String::from_utf8_lossy(&body_vec);

    let xml_doc = roxmltree::Document::parse(&body_string).unwrap();
    let elem = xml_doc
        .descendants()
        .find(|x| x.tag_name().name() == "ObjectID")
        .unwrap();
    let object_id = if let Ok(x) = elem.text().unwrap().to_string().parse::<u64>() {
        x
    } else {
        std::u64::MAX
    };

    if object_id == std::u64::MAX {
        return Ok(Response::builder()
            .status(hyper::StatusCode::NOT_FOUND)
            .body(Body::from("Lol".to_string()))
            .unwrap());
    }

    let mut response: Option<String> = None;

    let executed = {
        let mut locked_app_state = app_state.lock().unwrap();
        if let Some(list_items) = locked_app_state.cache.get_mut(&object_id) {
            response = Some(get_browse_response(list_items));
            true
        } else {
            false
        }
    };
    if !executed {
        let list_item = {
            let locked_app_state = app_state.lock().unwrap();
            locked_app_state.item_map.get(&object_id).unwrap().clone()
        };
        let id_counter = {
            let locked_app_state = app_state.lock().unwrap();
            locked_app_state.id_counter
        };
        let x = read_directory(
            hostname,
            list_item.dir.clone().unwrap(),
            object_id,
            id_counter,
        )
        .await;
        let list_items = x.list_items;
        {
            let mut locked_app_state = app_state.lock().unwrap();
            locked_app_state.id_counter = x.id_counter;
        }
        for item in &list_items {
            let mut locked_app_state = app_state.lock().unwrap();
            locked_app_state.item_map.insert(item.id, item.clone());
        }
        // let mut locked_app_state = app_state.lock().unwrap();
        // locked_app_state.cache.insert(object_id, list_items.clone());
        response = Some(get_browse_response(&list_items));
    }

    let response = response.unwrap();
    let response = r#"<?xml version="1.0" encoding="UTF-8"?>
    "#
    .to_string()
        + &response;

    Ok(Response::builder()
        .header("Content-Type", "text/xml")
        .body(Body::from(response))
        .unwrap())
}

pub async fn connection_desc_handler(_req: Request<Body>) -> anyhow::Result<Response<Body>> {
    Ok(Response::builder()
        .header("Content-Type", "text/xml")
        .body(Body::from(CONNECTION_DESC_XML.to_string()))
        .unwrap())
}

pub async fn connection_handler(req: Request<Body>) -> anyhow::Result<Response<Body>> {
    println!(
        "{}",
        String::from_utf8_lossy(&hyper::body::to_bytes(req).await.unwrap())
    );
    let response = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" xmlns="urn:schemas-upnp-org:service-1-0" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
   <s:Body>
      <u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1">
         <Source></Source>
         <Sink></Sink>
      </u:GetProtocolInfoResponse>
   </s:Body>
</s:Envelope>
"#;
    Ok(Response::builder()
        .header("Content-Type", "text/xml")
        .body(Body::from(response.to_string()))
        .unwrap())
}

#[derive(Debug)]
struct Conditionals {
    if_modified_since: Option<IfModifiedSince>,
    if_unmodified_since: Option<IfUnmodifiedSince>,
    if_range: Option<IfRange>,
    range: Option<Range>,
}

enum Cond {
    NoBody(Response<Body>),
    WithBody(Option<Range>),
}

impl Conditionals {
    fn check(self, last_modified: Option<LastModified>) -> Cond {
        if let Some(since) = self.if_unmodified_since {
            let precondition = last_modified
                .map(|time| since.precondition_passes(time.into()))
                .unwrap_or(false);

            tracing::trace!(
                "if-unmodified-since? {:?} vs {:?} = {}",
                since,
                last_modified,
                precondition
            );
            if !precondition {
                let mut res = Response::new(Body::empty());
                *res.status_mut() = StatusCode::PRECONDITION_FAILED;
                return Cond::NoBody(res);
            }
        }

        if let Some(since) = self.if_modified_since {
            tracing::trace!(
                "if-modified-since? header = {:?}, file = {:?}",
                since,
                last_modified
            );
            let unmodified = last_modified
                .map(|time| !since.is_modified(time.into()))
                // no last_modified means its always modified
                .unwrap_or(false);
            if unmodified {
                let mut res = Response::new(Body::empty());
                *res.status_mut() = StatusCode::NOT_MODIFIED;
                return Cond::NoBody(res);
            }
        }

        if let Some(if_range) = self.if_range {
            tracing::trace!("if-range? {:?} vs {:?}", if_range, last_modified);
            let can_range = !if_range.is_modified(None, last_modified.as_ref());

            if !can_range {
                return Cond::WithBody(None);
            }
        }

        Cond::WithBody(self.range)
    }
}

pub async fn serve_file(req: Request<Body>) -> anyhow::Result<Response<Body>> {
    let params: HashMap<String, String> = req
        .uri()
        .query()
        .map(|v| {
            url::form_urlencoded::parse(v.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_else(HashMap::new);

    let filename: std::path::PathBuf = (percent_decode_str(params.get("filename").unwrap()))
        .decode_utf8()
        .unwrap()
        .parse()
        .unwrap();
    let root: std::path::PathBuf = "/".parse().unwrap();

    let conditionals = Conditionals {
        if_modified_since: req.headers().typed_get(),
        if_range: req.headers().typed_get(),
        if_unmodified_since: req.headers().typed_get(),
        range: req.headers().typed_get(),
    };

    let file = File::open(filename.clone()).await?;

    file_conditional(file, filename, conditionals).await
}

async fn file_metadata(f: File) -> anyhow::Result<(File, Metadata)> {
    let meta = f.metadata().await?;
    Ok((f, meta))
}

struct BadRange;

fn bytes_range(range: Option<Range>, max_len: u64) -> Result<(u64, u64), BadRange> {
    use std::ops::Bound;

    let range = if let Some(range) = range {
        range
    } else {
        return Ok((0, max_len));
    };

    let ret = range
        .iter()
        .map(|(start, end)| {
            let start = match start {
                Bound::Unbounded => 0,
                Bound::Included(s) => s,
                Bound::Excluded(s) => s + 1,
            };

            let end = match end {
                Bound::Unbounded => max_len,
                Bound::Included(s) => {
                    // For the special case where s == the file size
                    if s == max_len {
                        s
                    } else {
                        s + 1
                    }
                }
                Bound::Excluded(s) => s,
            };

            if start < end && end <= max_len {
                Ok((start, end))
            } else {
                tracing::trace!("unsatisfiable byte range: {}-{}/{}", start, end, max_len);
                Err(BadRange)
            }
        })
        .next()
        .unwrap_or(Ok((0, max_len)));
    ret
}

fn reserve_at_least(buf: &mut BytesMut, cap: usize) {
    if buf.capacity() - buf.len() < cap {
        buf.reserve(cap);
    }
}

fn file_stream(
    mut file: File,
    buf_size: usize,
    (start, end): (u64, u64),
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    use std::io::SeekFrom;

    let seek = async move {
        if start != 0 {
            file.seek(SeekFrom::Start(start)).await?;
        }
        Ok(file)
    };

    seek.into_stream()
        .map(move |result| {
            let mut buf = BytesMut::new();
            let mut len = end - start;
            let mut f = match result {
                Ok(f) => f,
                Err(f) => return Either::Left(stream::once(future::err(f))),
            };

            Either::Right(stream::poll_fn(move |cx| {
                if len == 0 {
                    return Poll::Ready(None);
                }
                reserve_at_least(&mut buf, buf_size);

                let n = match ready!(poll_read_buf(Pin::new(&mut f), cx, &mut buf)) {
                    Ok(n) => n as u64,
                    Err(err) => {
                        tracing::debug!("file read error: {}", err);
                        return Poll::Ready(Some(Err(err)));
                    }
                };

                if n == 0 {
                    tracing::debug!("file read found EOF before expected length");
                    return Poll::Ready(None);
                }

                let mut chunk = buf.split().freeze();
                if n > len {
                    chunk = chunk.split_to(len as usize);
                    len = 0;
                } else {
                    len -= n;
                }

                Poll::Ready(Some(Ok(chunk)))
            }))
        })
        .flatten()
}

fn optimal_buf_size(metadata: &Metadata) -> usize {
    let block_size = get_block_size(metadata);

    // If file length is smaller than block size, don't waste space
    // reserving a bigger-than-needed buffer.
    std::cmp::min(block_size as u64, metadata.len()) as usize
}
const DEFAULT_READ_BUF_SIZE: usize = 8_192;
// const DEFAULT_READ_BUF_SIZE: usize = 2_048;

#[cfg(unix)]
fn get_block_size(metadata: &Metadata) -> usize {
    use std::os::unix::fs::MetadataExt;
    //TODO: blksize() returns u64, should handle bad cast...
    //(really, a block size bigger than 4gb?)

    // Use device blocksize unless it's really small.
    std::cmp::max(metadata.blksize() as usize, DEFAULT_READ_BUF_SIZE)
}

#[cfg(not(unix))]
fn get_block_size(_metadata: &Metadata) -> usize {
    DEFAULT_READ_BUF_SIZE
}

async fn file_conditional(
    f: File,
    path: PathBuf,
    conditionals: Conditionals,
) -> anyhow::Result<Response<Body>> {
    if let Ok((file, meta)) = file_metadata(f).await {
        let mut len = meta.len();
        let modified = meta.modified().ok().map(LastModified::from);

        let resp = match conditionals.check(modified) {
            Cond::NoBody(resp) => resp,
            Cond::WithBody(range) => {
                bytes_range(range, len)
                    .map(|(start, end)| {
                        let sub_len = end - start;
                        let buf_size = optimal_buf_size(&meta);
                        let stream = file_stream(file, buf_size, (start, end));
                        let body = Body::wrap_stream(stream);

                        let mut resp = Response::new(body);

                        if sub_len != len {
                            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
                            resp.headers_mut().typed_insert(
                                ContentRange::bytes(start..end, len).expect("valid ContentRange"),
                            );

                            len = sub_len;
                        }

                        let mime = mime_guess::from_path(path.clone()).first_or_octet_stream();

                        resp.headers_mut().typed_insert(ContentLength(len));
                        resp.headers_mut().typed_insert(ContentType::from(mime));
                        resp.headers_mut().typed_insert(AcceptRanges::bytes());

                        if let Some(last_modified) = modified {
                            resp.headers_mut().typed_insert(last_modified);
                        }

                        resp
                    })
                    .unwrap_or_else(|BadRange| {
                        // bad byte range
                        let mut resp = Response::new(Body::empty());
                        *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                        resp.headers_mut()
                            .typed_insert(ContentRange::unsatisfied_bytes(len));
                        resp
                    })
            }
        };

        Ok(resp)
    } else {
        bail!("Failed for not reading metadata")
    }
}
