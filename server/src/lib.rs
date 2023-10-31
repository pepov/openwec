#![allow(clippy::too_many_arguments)]

mod event;
mod formatter;
mod heartbeat;
mod kerberos;
mod logging;
mod logic;
mod multipart;
mod output;
mod outputs;
mod sldc;
mod soap;
mod subscription;
mod tls;

use anyhow::{anyhow, bail, Context, Result};
use common::database::{db_from_settings, schema_is_up_to_date, Db};
use common::encoding::decode_utf16le;
use common::settings::{Authentication, Kerberos, Tls};
use common::settings::{Collector, Server as ServerSettings, Settings};
use core::pin::Pin;
use futures::Future;
use futures_util::{future::join_all, StreamExt};
use heartbeat::{heartbeat_task, WriteHeartbeatMessage};
use http::response::Builder;
use http::status::StatusCode;
use hyper::body::{to_bytes, HttpBody};
use hyper::header::{CONTENT_TYPE, WWW_AUTHENTICATE};
use hyper::server::accept;
use hyper::server::conn::AddrIncoming;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use kerberos::AuthenticationError;
use lazy_static::lazy_static;
use libgssapi::error::MajorFlags;
use log::{debug, error, info, trace, warn};
use quick_xml::writer::Writer;
use regex::Regex;
use soap::Serializable;
use std::boxed::Box;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::ready;
use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use std::{env, mem};
use subscription::{reload_subscriptions_task, Subscriptions};
use tls_listener::TlsListener;
use tokio::signal::unix::SignalKind;
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::logging::ACCESS_LOGGER;
use crate::tls::{make_config, subject_from_cert};

pub enum RequestCategory {
    Enumerate(String),
    Subscription(String),
}

impl TryFrom<&Request<Body>> for RequestCategory {
    type Error = anyhow::Error;
    fn try_from(req: &Request<Body>) -> Result<Self, Self::Error> {
        if req.method() != "POST" {
            bail!("Invalid HTTP method {}", req.method());
        }

        lazy_static! {
            static ref SUBSCRIPTION_RE: Regex = Regex::new(r"^/wsman/subscriptions/([0-9A-Fa-f]{8}\b-[0-9A-Fa-f]{4}\b-[0-9A-Fa-f]{4}\b-[0-9A-Fa-f]{4}\b-[0-9A-F]{12})$").expect("Failed to compile SUBSCRIPTION regular expression");
        }
        if let Some(c) = SUBSCRIPTION_RE.captures(req.uri().path()) {
            return Ok(RequestCategory::Subscription(
                c.get(1)
                    .ok_or_else(|| anyhow!("Could not get identifier from URI"))?
                    .as_str()
                    .to_owned(),
            ));
        }

        return Ok(Self::Enumerate(req.uri().to_string()));
    }
}

pub struct RequestData {
    principal: String,
    remote_addr: SocketAddr,
    category: RequestCategory,
}

impl RequestData {
    fn new(principal: &str, remote_addr: &SocketAddr, req: &Request<Body>) -> Result<Self> {
        Ok(RequestData {
            principal: principal.to_owned(),
            remote_addr: remote_addr.to_owned(),
            category: RequestCategory::try_from(req)?,
        })
    }

    /// Get a reference to the request data's principal.
    pub fn principal(&self) -> &str {
        self.principal.as_ref()
    }

    /// Get a reference to the request data's remote addr.
    pub fn remote_addr(&self) -> &SocketAddr {
        &self.remote_addr
    }

    /// Get a reference to the request data's category.
    pub fn category(&self) -> &RequestCategory {
        &self.category
    }
}

#[derive(Debug, Clone)]
/// Kerberos : state
/// Tls : subject, thumbprint
pub enum AuthenticationContext {
    Kerberos(Arc<Mutex<kerberos::State>>),
    Tls(String, String),
}

async fn get_request_payload(
    collector: &Collector,
    auth_ctx: &AuthenticationContext,
    req: Request<Body>,
) -> Result<Option<String>> {
    let (parts, body) = req.into_parts();

    let response_content_length = body
        .size_hint()
        .upper()
        .ok_or_else(|| anyhow!("Header Content-Length is not present"))
        .context("Could not check Content-Length header of request")?;

    let max_content_length = collector.max_content_length();

    if response_content_length > max_content_length {
        bail!(
            "HTTP request body is too large ({} bytes larger than the maximum allowed {} bytes).",
            response_content_length,
            max_content_length
        );
    }

    let data = to_bytes(body)
        .await
        .context("Could not retrieve request body")?;

    if data.is_empty() {
        return Ok(None);
    }

    let message = match auth_ctx {
        AuthenticationContext::Tls(_, _) => tls::get_request_payload(parts, data).await?,
        AuthenticationContext::Kerberos(conn_state) => {
            kerberos::get_request_payload(conn_state, parts, data).await?
        }
    };

    match message {
        Some(bytes) => Ok(Some(decode_utf16le(bytes)?)),
        _ => Ok(None),
    }
}

fn create_response(
    auth_ctx: &AuthenticationContext,
    mut response: Builder,
    payload: Option<String>,
) -> Result<Response<Body>> {
    match auth_ctx {
        AuthenticationContext::Tls(_, _) => {
            if payload.is_some() {
                response = response.header(CONTENT_TYPE, "application/soap+xml;charset=UTF-16");
            }
            let body = match payload {
                None => Body::empty(),
                Some(payload) => Body::from(
                    tls::get_response_payload(payload).context("Failed to compute TLS payload")?,
                ),
            };
            Ok(response.body(body)?)
        }
        AuthenticationContext::Kerberos(conn_state) => {
            let boundary = "Encrypted Boundary";
            if payload.is_some() {
                response = response.header(CONTENT_TYPE, "multipart/encrypted;protocol=\"application/HTTP-Kerberos-session-encrypted\";boundary=\"".to_owned() + boundary + "\"");
            }
            let body = match payload {
                None => Body::empty(),
                Some(payload) => Body::from(
                    kerberos::get_response_payload(conn_state, payload, boundary)
                        .context("Failed to compute Kerberos encrypted payload")?,
                ),
            };
            Ok(response.body(body)?)
        }
    }
}

fn log_auth_error(addr: &SocketAddr, req: &Request<Body>, err_str: String, do_warn: bool) {
    let str_format = format!(
        "Authentication failed for {}:{} ({}:{}): {}",
        addr.ip(),
        addr.port(),
        req.method(),
        req.uri(),
        err_str.replace('\n', " ")
    );

    if do_warn {
        warn!("{}", str_format);
    } else {
        debug!("{}", str_format);
    }
}

async fn authenticate(
    auth_ctx: &AuthenticationContext,
    req: &Request<Body>,
    addr: &SocketAddr,
) -> Result<(String, Builder)> {
    match auth_ctx {
        AuthenticationContext::Tls(subject, _) => {
            // if subject is empty, show unauthorized error
            if subject.is_empty() {
                log_auth_error(addr, req, "Empty certificate".to_owned(), true);
                bail!("Empty certificate")
            }

            let response = Response::builder();
            Ok((subject.to_owned(), response))
        }
        AuthenticationContext::Kerberos(conn_state) => {
            let mut response = Response::builder();
            let auth_result = kerberos::authenticate(conn_state, req)
                .await
                .map_err(|err| {
                    match err {
                        AuthenticationError::Gssapi(gssapi_err)
                            if gssapi_err.major.bits()
                                != MajorFlags::GSS_S_CONTEXT_EXPIRED.bits() =>
                        {
                            log_auth_error(addr, req, format!("{:?}", err), true)
                        }
                        AuthenticationError::Other(_) => {
                            log_auth_error(addr, req, format!("{:?}", err), true)
                        }
                        _ => log_auth_error(addr, req, format!("{:?}", err), false),
                    }
                    err
                })?;
            if let Some(token) = auth_result.token() {
                response = response.header(WWW_AUTHENTICATE, format!("Kerberos {}", token))
            }
            Ok((auth_result.principal().to_owned(), response))
        }
    }
}

async fn handle_payload(
    server: &ServerSettings,
    collector: &Collector,
    db: Db,
    subscriptions: Subscriptions,
    heartbeat_tx: mpsc::Sender<WriteHeartbeatMessage>,
    request_data: RequestData,
    request_payload: Option<String>,
    auth_ctx: &AuthenticationContext,
) -> Result<(StatusCode, Option<String>)> {
    match request_payload {
        None => Ok((StatusCode::OK, None)),
        Some(payload) => {
            let message = soap::parse(&payload).context("Failed to parse SOAP message")?;
            trace!("Parsed request: {:?}", message);
            let response = logic::handle_message(
                server,
                collector,
                db,
                subscriptions,
                heartbeat_tx,
                request_data,
                &message,
                auth_ctx,
            )
            .await
            .context("Failed to handle SOAP message")?;

            match response {
                logic::Response::Err(status_code) => Ok((status_code, None)),
                logic::Response::Ok(action, body) => {
                    let payload = soap::Message::response_from(&message, &action, body)
                        .context("Failed to build a response payload")?;
                    let mut writer = Writer::new(Cursor::new(Vec::new()));
                    payload
                        .serialize(&mut writer)
                        .context("Failed to serialize response payload")?;
                    let result = String::from_utf8(writer.into_inner().into_inner())?;
                    trace!("Response is: {}", result);
                    Ok((StatusCode::OK, Some(result)))
                }
            }
        }
    }
}

fn log_response(
    addr: &SocketAddr,
    method: &str,
    uri: &str,
    start: &Instant,
    status: StatusCode,
    principal: &str,
) {
    let duration: f32 = start.elapsed().as_micros() as f32;
    // MDC is thread related, so it should be safe to use it in a non-async
    // function.
    log_mdc::insert("http_status", status.as_str());
    log_mdc::insert("http_method", method);
    log_mdc::insert("http_uri", uri);
    log_mdc::insert("response_time", format!("{:.3}", duration / 1000.0));
    log_mdc::insert("ip", addr.ip().to_string());
    log_mdc::insert("port", addr.port().to_string());
    log_mdc::insert("principal", principal);

    // Empty message, logging pattern should use MDC
    info!(target: ACCESS_LOGGER, "");
    log_mdc::clear();
}

async fn handle(
    server: ServerSettings,
    collector: Collector,
    db: Db,
    subscriptions: Subscriptions,
    heartbeat_tx: mpsc::Sender<WriteHeartbeatMessage>,
    auth_ctx: AuthenticationContext,
    addr: SocketAddr,
    req: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    let start = Instant::now();

    debug!(
        "Received HTTP request from {}:{}: {} {}",
        addr.ip(),
        addr.port(),
        req.method(),
        req.uri()
    );

    let method = req.method().to_string();
    let uri = req.uri().to_string();

    // Check authentication
    let (principal, mut response_builder) = match authenticate(&auth_ctx, &req, &addr).await {
        Ok((principal, builder)) => (principal, builder),
        Err(_) => {
            let status = StatusCode::UNAUTHORIZED;
            log_response(&addr, &method, &uri, &start, status, "-");
            return Ok(Response::builder()
                .status(status)
                .body(Body::empty())
                .expect("Failed to build HTTP response"));
        }
    };

    debug!("Successfully authenticated {}", principal);

    let request_data = match RequestData::new(&principal, &addr, &req) {
        Ok(request_data) => request_data,
        Err(e) => {
            error!("Failed to compute request data: {:?}", e);
            let status = StatusCode::NOT_FOUND;
            log_response(&addr, &method, &uri, &start, status, &principal);
            return Ok(Response::builder()
                .status(status)
                .body(Body::empty())
                .expect("Failed to build HTTP response"));
        }
    };

    // Get request payload
    let request_payload = match get_request_payload(&collector, &auth_ctx, req).await {
        Ok(payload) => payload,
        Err(e) => {
            error!("Failed to retrieve request payload: {:?}", e);
            let status = StatusCode::BAD_REQUEST;
            log_response(&addr, &method, &uri, &start, status, &principal);
            return Ok(Response::builder()
                .status(status)
                .body(Body::empty())
                .expect("Failed to build HTTP response"));
        }
    };

    trace!(
        "Received payload: {:?}",
        request_payload.as_ref().unwrap_or(&String::from(""))
    );

    // Handle request payload, and retrieves response payload
    let (status, response_payload) = match handle_payload(
        &server,
        &collector,
        db,
        subscriptions,
        heartbeat_tx,
        request_data,
        request_payload,
        &auth_ctx,
    )
    .await
    {
        Ok((status, response_payload)) => (status, response_payload),
        Err(e) => {
            error!("Failed to compute a response payload to request: {:?}", e);
            let status = StatusCode::INTERNAL_SERVER_ERROR;
            log_response(&addr, &method, &uri, &start, status, &principal);
            return Ok(Response::builder()
                .status(status)
                .body(Body::empty())
                .expect("Failed to build HTTP response"));
        }
    };

    trace!(
        "Send response {} with payload: {:?}",
        status,
        response_payload
    );

    response_builder = response_builder.status(status);
    // Create HTTP response
    let response = match create_response(&auth_ctx, response_builder, response_payload) {
        Ok(response) => response,
        Err(e) => {
            error!("Failed to build HTTP response: {:?}", e);
            let status = StatusCode::INTERNAL_SERVER_ERROR;
            log_response(&addr, &method, &uri, &start, status, &principal);
            return Ok(Response::builder()
                .status(status)
                .body(Body::empty())
                .expect("Failed to build HTTP response"));
        }
    };

    log_response(&addr, &method, &uri, &start, response.status(), &principal);
    Ok(response)
}

fn create_kerberos_server(
    kerberos_settings: &Kerberos,
    collector_settings: Collector,
    collector_db: Db,
    collector_subscriptions: Subscriptions,
    collector_heartbeat_tx: mpsc::Sender<WriteHeartbeatMessage>,
    collector_server_settings: ServerSettings,
    addr: SocketAddr,
) -> Pin<Box<dyn Future<Output = hyper::Result<()>> + Send>> {
    let principal = kerberos_settings.service_principal_name().to_owned();
    // Try to initialize a security context. This is to be sure that an error in
    // Kerberos configuration will be reported as soon as possible.
    let state = kerberos::State::new(&principal);
    if state.context_is_none() {
        panic!("Could not initialize Kerberos context");
    }

    let tcp_keepalive_time = Duration::from_secs(collector_server_settings.tcp_keepalive_time());
    let tcp_keepalive_interval = collector_server_settings
        .tcp_keepalive_intvl()
        .map(Duration::from_secs);
    let tcp_keepalive_probes = collector_server_settings.tcp_keepalive_probes();

    // A `MakeService` that produces a `Service` to handle each connection.
    let make_service = make_service_fn(move |conn: &AddrStream| {
        // We have to clone the context to share it with each invocation of
        // `make_service`.

        // Initialize Kerberos context once for each TCP connection
        let collector_settings = collector_settings.clone();
        let svc_db = collector_db.clone();
        let svc_server_settings = collector_server_settings.clone();
        let auth_ctx =
            AuthenticationContext::Kerberos(Arc::new(Mutex::new(kerberos::State::new(&principal))));
        let subscriptions = collector_subscriptions.clone();
        let collector_heartbeat_tx = collector_heartbeat_tx.clone();

        let addr = conn.remote_addr();

        debug!("Received TCP connection from {}", addr);

        // Create a `Service` for responding to the request.
        let service = service_fn(move |req| {
            handle(
                svc_server_settings.clone(),
                collector_settings.clone(),
                svc_db.clone(),
                subscriptions.clone(),
                collector_heartbeat_tx.clone(),
                auth_ctx.clone(),
                addr,
                req,
            )
        });

        // Return the service to hyper.
        async move { Ok::<_, Infallible>(service) }
    });

    // Then bind and serve...
    let server = Server::bind(&addr)
        .tcp_keepalive(Some(tcp_keepalive_time))
        .tcp_keepalive_interval(tcp_keepalive_interval)
        .tcp_keepalive_retries(tcp_keepalive_probes)
        .serve(make_service)
        .with_graceful_shutdown(http_shutdown_signal());

    info!("Server listenning on {}", addr);
    // XXX : because the 2 closures have different types we use this, but may be better way to do this
    Box::pin(server)
}

fn create_tls_server(
    tls_settings: &Tls,
    collector_settings: Collector,
    collector_db: Db,
    collector_subscriptions: Subscriptions,
    collector_heartbeat_tx: mpsc::Sender<WriteHeartbeatMessage>,
    collector_server_settings: ServerSettings,
    addr: SocketAddr,
) -> Pin<Box<dyn Future<Output = hyper::Result<()>> + Send>> {
    // make TLS connection config
    let tls_config = make_config(tls_settings).expect("Error while configuring server");

    let tcp_keepalive_time = Duration::from_secs(collector_server_settings.tcp_keepalive_time());
    let tcp_keepalive_interval = collector_server_settings
        .tcp_keepalive_intvl()
        .map(Duration::from_secs);
    let tcp_keepalive_probes = collector_server_settings.tcp_keepalive_probes();

    // create the service per connection
    let make_service = make_service_fn(move |conn: &TlsStream<AddrStream>| {
        // get peer certificate (= user certificate)
        let cert = conn
            .get_ref()
            .1
            .peer_certificates()
            .expect("Peer certificate should exist") // client auth has to happen, so this should not fail
            .first()
            .expect("Peer certificate should not be empty") // client cert cannot be empty if authentication succeeded
            .clone();

        let subject = subject_from_cert(cert.as_ref()).expect("Could not parse client certificate");
        let thumbprint = tls_config.thumbprint.clone();

        let collector_settings = collector_settings.clone();
        let svc_db = collector_db.clone();
        let svc_server_settings = collector_server_settings.clone();
        let subscriptions = collector_subscriptions.clone();
        let collector_heartbeat_tx = collector_heartbeat_tx.clone();

        let addr = conn.get_ref().0.remote_addr();
        let auth_ctx = AuthenticationContext::Tls(subject, thumbprint);

        // create service per request
        let service = service_fn(move |req| {
            handle(
                svc_server_settings.clone(),
                collector_settings.clone(),
                svc_db.clone(),
                subscriptions.clone(),
                collector_heartbeat_tx.clone(),
                auth_ctx.clone(),
                addr,
                req,
            )
        });

        async move { Ok::<_, Infallible>(service) }
    });

    // create acceptor from config
    let tls_acceptor: TlsAcceptor = tls_config.server.into();

    let mut addr_incoming = AddrIncoming::bind(&addr).expect("Could not bind address to listener");
    addr_incoming.set_keepalive(Some(tcp_keepalive_time));
    addr_incoming.set_keepalive_interval(tcp_keepalive_interval);
    addr_incoming.set_keepalive_retries(tcp_keepalive_probes);

    // configure listener on the address to use the acceptor
    let incoming = TlsListener::new(tls_acceptor, addr_incoming)
        .connections()
        .filter(|conn| {
            if let Err(err) = &conn {
                match err {
                    tls_listener::Error::TlsAcceptError { error, .. }
                        if error.to_string() == "tls handshake eof" =>
                    {
                        // happens sometimes, not problematic
                        debug!("Error while establishing a connection: {:?}", err)
                    }
                    _ => warn!("Error while establishing a connection: {:?}", err),
                };
                ready(false)
            } else {
                ready(true)
            }
        });

    let server = Server::builder(accept::from_stream(incoming))
        .serve(make_service)
        .with_graceful_shutdown(http_shutdown_signal());

    info!("Server listenning on {}", addr);
    // XXX : because the 2 closures have different types we use this, but may be better way to do this
    Box::pin(server)
}

enum ShutdownReason {
    CtrlC,
    Sigterm,
}

async fn shutdown_signal() -> ShutdownReason {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())
        .expect("failed to install SIGTERM handler");

    tokio::select! {
        _ = ctrl_c => ShutdownReason::CtrlC,
        _ = sigterm.recv() => ShutdownReason::Sigterm,
    }
}

async fn http_shutdown_signal() {
    shutdown_signal().await;
}

async fn force_shutdown_timeout() {
    match shutdown_signal().await {
        ShutdownReason::CtrlC => {
            info!("Received CTRL+C")
        }
        ShutdownReason::Sigterm => {
            info!("Received SIGTERM signal")
        }
    }
    debug!("Start 10 secs timeout before killing HTTP servers");
    tokio::time::sleep(Duration::from_secs(10)).await;
}

pub async fn run(settings: Settings, verbosity: u8) {
    // Initialize loggers
    if let Err(e) = logging::init(&settings, verbosity) {
        panic!("Failed to setup logging: {:?}", e);
    }

    // XXX : because the 2 closures have different types we use this, but may be better way to do this
    let mut servers: Vec<Pin<Box<dyn Future<Output = hyper::Result<()>> + Send>>> = Vec::new();

    let db: Db = db_from_settings(&settings)
        .await
        .expect("Failed to initialize database");

    // Check that database schema is up to date
    match schema_is_up_to_date(db.clone()).await {
        Ok(true) => (),
        Ok(false) => panic!("Schema needs to be updated. Please check migration guide and then run `openwec db upgrade`"),
        Err(err) => panic!("An error occurred while checking schema version: {:?}.\nHelp: You may need to run `openwec db init` to setup your database.", err),
    };

    let subscriptions = Arc::new(RwLock::new(HashMap::new()));

    let interval = settings.server().db_sync_interval();
    let update_task_db = db.clone();
    let update_task_subscriptions = subscriptions.clone();
    // Launch a task responsible for updating subscriptions
    tokio::spawn(async move {
        reload_subscriptions_task(update_task_db, update_task_subscriptions, interval).await
    });

    // To reduce database load, heartbeats are not saved immediately.
    // Heartbeats data "to store" are cached in memory before being saved in database periodically.
    // To "store" a heartbeat, request handlers send a message to the heartbeat task
    // using a MPSC channel.
    // The database store operation may take some time. During this operation, new heartbeats message
    // are not popped from the channel. The channel must be large enough to enable the request handlers to enqueue
    // their heartbeat messages without waiting.
    let interval = settings.server().flush_heartbeats_interval();
    let update_task_db = db.clone();
    let (heartbeat_tx, heartbeat_rx) =
        mpsc::channel(settings.server().heartbeats_queue_size() as usize);

    // We use a CancellationToken to tell the task to shutdown, so
    // that it is able to store cached heartbeats.
    let heartbeat_ct = CancellationToken::new();
    let cloned_heartbaat_ct = heartbeat_ct.clone();

    // Launch the task responsible for managing heartbeats
    let heartbeat_task = tokio::spawn(async move {
        heartbeat_task(update_task_db, interval, heartbeat_rx, cloned_heartbaat_ct).await
    });

    // Set KRB5_KTNAME env variable if necessary (i.e. if at least one collector uses
    // Kerberos authentication)
    if settings.collectors().iter().any(|x| {
        mem::discriminant(x.authentication())
            == mem::discriminant(&Authentication::Kerberos(Kerberos::empty()))
    }) {
        env::set_var(
            "KRB5_KTNAME",
            settings
                .server()
                .keytab()
                .expect("Kerberos authentication requires the server.keytab setting to be set"),
        );
    }

    info!("Server settings: {:?}", settings.server());

    for collector in settings.collectors() {
        let collector_db = db.clone();
        let collector_subscriptions = subscriptions.clone();
        let collector_settings = collector.clone();
        let collector_heartbeat_tx = heartbeat_tx.clone();
        let collector_server_settings = settings.server().clone();

        // Construct our SocketAddr to listen on...
        let addr = SocketAddr::from((
            IpAddr::from_str(collector.listen_address())
                .expect("Failed to parse server.listen_address"),
            collector.listen_port(),
        ));

        trace!("Listen address is {}", addr);

        // create server depending on connection type it allows
        match collector.authentication() {
            Authentication::Kerberos(kerberos) => {
                servers.push(create_kerberos_server(
                    kerberos,
                    collector_settings,
                    collector_db,
                    collector_subscriptions,
                    collector_heartbeat_tx,
                    collector_server_settings,
                    addr,
                ));
            }

            Authentication::Tls(tls) => {
                servers.push(create_tls_server(
                    tls,
                    collector_settings,
                    collector_db,
                    collector_subscriptions,
                    collector_heartbeat_tx,
                    collector_server_settings,
                    addr,
                ));
            }
        };
    }

    tokio::select! {
        _ = force_shutdown_timeout() => {
            warn!("HTTP servers graceful shutdown timed out.");
        },
        result = join_all(servers) => {
            for server in result {
                if let Err(e) = server {
                    error!("HTTP server error: {}", e);
                }
            }
            info!("HTTP servers have been shutdown gracefully.");
        }
    }

    // Signal the task that we want to shutdown
    heartbeat_ct.cancel();
    // Wait for the task to shutdown gracefully
    if let Err(e) = heartbeat_task.await {
        error!("Failed to wait for heartbeat task to shutdown: {:?}", e)
    }
}
