use hyper::{Body, Request, Response, Server, StatusCode};
// Import the routerify prelude traits.
use pnet::datalink;
use routerify::prelude::*;
use routerify::{Middleware, RequestInfo, Router, RouterService};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::{convert::Infallible, net::SocketAddr};
use tokio::net::UdpSocket;
use tracing::{info, Span};
use tracing::{instrument, Instrument};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use uuid::Uuid;

mod handlers;
mod types;
mod utils;

// Define an app state to share it across the route handlers and middlewares.
struct State(u64);

macro_rules! traceroute {
    ($fn: expr) => {
        |req| async move {
            let span = req.extensions().get::<Span>().cloned();
            match span {
                Some(x) => $fn(req).instrument(x).await,
                None => $fn(req).await,
            }
        }
    };
}

// A handler for "/" page.
#[tracing::instrument(skip(req))]
async fn home_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    // Access the app state.
    let state = req.data::<State>().unwrap();
    let span = req.extensions().get::<Span>().cloned();

    println!("State value: {}", state.0);
    info!("Orphan event without a parent span");
    let mut response = Response::new(Body::from("Home page"));
    response.extensions_mut().insert(span);
    Ok(response)
}

// A handler for "/users/:userId" page.
async fn user_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let user_id = req.param("userId").unwrap();
    Ok(Response::new(Body::from(format!("Hello {}", user_id))))
}

// A middleware which logs an http request.
async fn logger(mut req: Request<Body>) -> anyhow::Result<Request<Body>> {
    let request_id = Uuid::new_v4();
    let span =
        tracing::info_span!("Http Request", http.method = %req.method(), request_id = %request_id);
    req.extensions_mut().insert(span.clone());
    println!(
        "{} {} {}",
        req.remote_addr(),
        req.method(),
        req.uri().path()
    );
    Ok(req)
}

async fn logger2(res: Response<Body>, req: RequestInfo) -> anyhow::Result<Response<Body>> {
    Ok(res)
}

// Define an error handler function which will accept the `routerify::Error`
// and the request information and generates an appropriate response.
async fn error_handler(err: routerify::RouteError, _: RequestInfo) -> Response<Body> {
    eprintln!("{}", err);
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from(format!("Something went wrong: {}", err)))
        .unwrap()
}

// Create a `Router<Body, Infallible>` for response body type `hyper::Body`
// and for handler error type `Infallible`.
fn router(app_uuid: AppUuid) -> Router<Body, anyhow::Error> {
    let mut cache = get_cache();
    let mut item_map = HashMap::new();
    for items in cache.get_mut(&0) {
        for item in items {
            item_map.insert(item.id, item.clone());
        }
    }

    let id_counter = (cache.get_mut(&0).unwrap().len() + 1) as u64;

    let app_state = AppState {
        cache,
        id_counter,
        item_map,
        uuid: app_uuid.as_ref().clone(),
    };
    let app_state = Arc::new(Mutex::new(app_state));

    Router::builder()
        .data(State(100))
        .data(app_uuid)
        .data(app_state)
        .middleware(Middleware::pre(logger))
        .middleware(Middleware::post_with_info(logger2))
        .get("/root.xml", handlers::root_handler)
        .get("/content/desc.xml", handlers::content_desc_handler)
        .post("/content/control", handlers::content_handler)
        .get("/connection/desc.xml", handlers::connection_desc_handler)
        .post("/connection/control", handlers::connection_handler)
        .get("/agni-files/", handlers::serve_file)
        .err_handler_with_info(error_handler)
        .build()
        .unwrap()
}

use std::io;

use crate::types::{AppState, AppUuid};
use crate::utils::{get_cache, get_multicast_ssdp_socket_v4};

pub fn reply_message(ip: IpAddr, uuid: Uuid, st: &str) -> Vec<u8> {
    let date = chrono::offset::Utc::now().format("%a, %m %b %Y %H:%M:%S GMT");
    let uuid_str = uuid.to_string();
    let usn = format!("{}::{}", uuid_str, st);

    format!(
        "\
HTTP/1.1 200 OK\r\n\
CACHE-CONTROL: max-age=1800\r\n\
DATE: {date}\r\n\
EXT:\r\n\
LOCATION: http://{ip}:3030/root.xml\r\n\
SERVER: Linux/5.8, UPnP/1.0, agni/1.0\r\n\
ST: {st}\r\n\
USN: {usn}\r\n\
\r\n",
        date = date,
        ip = ip,
        st = st,
        usn = usn
    )
    .into_bytes()
}

pub fn broadcast_message(ip: IpAddr, uuid: Uuid, nt: &str) -> Vec<u8> {
    let uuid_str = uuid.to_string();
    let usn = format!("{}::{}", uuid_str, nt);

    format!(
        "\
NOTIFY * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
NT: {nt}\r\n\
NTS: ssdp:alive\r\n\
LOCATION: http://{ip}:3030/root.xml\r\n\
USN: {usn}\r\n\
CACHE-CONTROL: max-age=1800\r\n\
SERVER: Linux/5.8, UPnP/1.0, agni/1.0\r\n\
\r\n",
        nt = nt,
        ip = ip,
        usn = usn
    )
    .into_bytes()
}

pub async fn reply_presence(uuid: Uuid, unicast_ip: SocketAddr) {
    let ips = utils::get_local_ips();
    for ip in ips {
        let msg_uuid = reply_message(ip, uuid, &uuid.to_string());
        let msg_root = reply_message(ip, uuid, "upnp:rootdevice");
        let msg_mediaserver = reply_message(ip, uuid, "urn:schemas-upnp-org:device:MediaServer:1");
        let msg_content_directory =
            reply_message(ip, uuid, "urn:schemas-upnp-org:service:ContentDirectory:1");
        let msg_connection_manager =
            reply_message(ip, uuid, "urn:schemas-upnp-org:service:ConnectionManager:1");

        let unicast_ip_string = Some(unicast_ip.to_string());

        for _ in 0..3i32 {
            broadcast_packet(&msg_uuid, &unicast_ip_string).await;
            broadcast_packet(&msg_root, &unicast_ip_string).await;
            broadcast_packet(&msg_mediaserver, &unicast_ip_string).await;
            broadcast_packet(&msg_content_directory, &unicast_ip_string).await;
            broadcast_packet(&msg_connection_manager, &unicast_ip_string).await;
        }
    }
}

pub async fn broadcast_presence(uuid: Uuid) {
    let ips = utils::get_local_ips();
    for ip in ips {
        let msg_uuid = broadcast_message(ip, uuid, &uuid.to_string());
        let msg_root = broadcast_message(ip, uuid, "upnp:rootdevice");
        let msg_mediaserver =
            broadcast_message(ip, uuid, "urn:schemas-upnp-org:device:MediaServer:1");
        let msg_content_directory =
            broadcast_message(ip, uuid, "urn:schemas-upnp-org:service:ContentDirectory:1");
        let msg_connection_manager =
            broadcast_message(ip, uuid, "urn:schemas-upnp-org:service:ConnectionManager:1");

        for _ in 0..3i32 {
            broadcast_packet(&msg_uuid, &None).await;
            broadcast_packet(&msg_root, &None).await;
            broadcast_packet(&msg_mediaserver, &None).await;
            broadcast_packet(&msg_content_directory, &None).await;
            broadcast_packet(&msg_connection_manager, &None).await;
        }
    }
}

pub async fn broadcast_packet<'a>(data: &'a [u8], unicast_ip: &Option<String>) {
    let socket = UdpSocket::bind("[::]:0").await.unwrap();
    let addr = if let Some(x) = unicast_ip {
        x.to_owned()
    } else {
        "239.255.255.250:1900".to_string()
    };
    if !addr.contains("239.255.255.250:1900") {
        println!(
            "To Address: {:?}\nMessage:\n {:?}\n",
            addr,
            String::from_utf8_lossy(data)
        );
    }
    socket
        .send_to(data, addr)
        .await
        .map(|bytes_written| {
            if bytes_written != data.len() {
                eprintln!("Failed to write bytes");
            }
        })
        .unwrap();
}

pub fn initialize_tracing() {
    let formatting_layer = BunyanFormattingLayer::new("agni".into(), std::io::stdout);
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = Registry::default()
        .with(JsonStorageLayer)
        .with(env_filter)
        .with(formatting_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    initialize_tracing();

    let uuid = AppUuid::new(Uuid::new_v4());

    let router = router(uuid);

    // Create a Service from the router above to handle incoming requests.
    let service = RouterService::new(router).unwrap();

    // The address on which the server will be listening.
    let addr = SocketAddr::from(([0, 0, 0, 0], 3030));

    // Create a server by passing the created service to `.serve` method.
    let server = Server::bind(&addr).serve(service);

    let handle1 = tokio::spawn(async move {
        let socket = get_multicast_ssdp_socket_v4().unwrap();

        loop {
            let mut buf = [0; 2048];

            match socket.recv_from(&mut buf).await {
                Ok((received, addr)) => {
                    let s = String::from_utf8_lossy(&buf[..received]);
                    if s.contains("M-SEARCH")
                        && s.contains("ssdp:discover")
                        && (s.contains("MediaServer") || s.contains("ContentDirectory"))
                    {
                        reply_presence(uuid.as_ref().clone(), addr).await;
                    }
                }
                Err(e) => eprintln!("recv function failed: {:?}", e),
            };
        }
    });

    let handle2 = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(2000));
        loop {
            interval.tick().await;
            broadcast_presence(uuid.as_ref().clone()).await;
        }
    });

    let (_, _, _) = tokio::join!(handle1, handle2, server);
    Ok(())
}
