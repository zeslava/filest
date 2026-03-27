use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use hyper::server::conn::http2;
use serde::{Deserialize, Serialize};
use std::{io::BufReader, path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::ServiceBuilder;
use tower::util::ServiceExt;
use tower_http::trace::TraceLayer;

use bytes::Buf;
use std::collections::HashMap;

#[derive(Clone)]
struct AppState {
    namespaces: Arc<HashMap<String, PathBuf>>,
}

#[tokio::main]
async fn main() {
    let _ = dotenv::dotenv();
    tracing_subscriber::fmt::init();

    let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8090".to_string());

    let mut namespaces = HashMap::new();
    for (key, value) in std::env::vars() {
        if let Some(name) = key.strip_prefix("NS_") {
            let name = name.to_lowercase();
            let path = PathBuf::from(&value)
                .canonicalize()
                .unwrap_or_else(|_| panic!("NS_{} path does not exist: {}", name, value));
            if !path.is_dir() {
                panic!("NS_{} is not a directory: {}", name, value);
            }
            tracing::info!("Namespace '{}' -> {}", name, path.display());
            namespaces.insert(name, path);
        }
    }
    if namespaces.is_empty() {
        panic!("No namespaces configured. Set NS_<name>=<path> env vars.");
    }

    let state = AppState { namespaces: Arc::new(namespaces) };

    let app = Router::new()
        .route("/{*path}", get(get_or_list).put(put_file).delete(delete_file).patch(patch_rename))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(state);

    let cert_path = std::env::var("CERT_PATH").ok();
    let key_path = std::env::var("KEY_PATH").ok();

    // Build QUIC certs: use provided certs or generate self-signed
    let (quic_certs, quic_key) = match (&cert_path, &key_path) {
        (Some(cp), Some(kp)) => load_certs_from_files(cp, kp),
        _ => {
            tracing::info!("No TLS certs provided, generating self-signed for HTTP/3");
            generate_self_signed_cert()
        }
    };

    // Start HTTP/3 (QUIC) server on same port (UDP)
    let addr: std::net::SocketAddr = listen_addr.parse().expect("Invalid LISTEN_ADDR");
    let quic_config = build_quic_server_config(quic_certs, quic_key);
    let endpoint = quinn::Endpoint::server(quic_config, addr)
        .expect("Failed to bind QUIC endpoint");
    tokio::spawn(serve_h3(endpoint, app.clone()));

    // Start TCP server (HTTP/1.1 + HTTP/2)
    match (cert_path, key_path) {
        (Some(cert), Some(key)) => {
            serve_tls(app, &listen_addr, &cert, &key).await;
        }
        _ => {
            let listener = TcpListener::bind(&listen_addr)
                .await
                .unwrap_or_else(|e| panic!("Failed to bind to {listen_addr}: {e}"));
            tracing::info!("filestore-api listening on http://{listen_addr}");
            axum::serve(listener, app).await.unwrap();
        }
    }
}

async fn serve_tls(app: Router, listen_addr: &str, cert_path: &str, key_path: &str) {
    let cert_file = std::fs::File::open(cert_path).expect("Failed to open cert file");
    let key_file = std::fs::File::open(key_path).expect("Failed to open key file");

    let certs = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut keys: Vec<_> = rustls_pemfile::pkcs8_private_keys(&mut BufReader::new(key_file))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    if keys.is_empty() {
        panic!("No private key found in {key_path}");
    }

    let mut config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            certs,
            tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(keys.remove(0)),
        )
        .unwrap();

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let tls_acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind(listen_addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind to {listen_addr}: {e}"));

    tracing::info!("filestore-api listening on https://{listen_addr}");

    loop {
        match listener.accept().await {
            Ok((socket, _peer)) => {
                let acceptor = tls_acceptor.clone();
                let app = app.clone();
                tokio::spawn(async move {
                    let Ok(tls_stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let alpn = tls_stream.get_ref().1.alpn_protocol().map(|p| p.to_vec());
                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let service = hyper::service::service_fn(move |req| {
                        let app = app.clone();
                        async move { Ok::<_, std::io::Error>(app.clone().oneshot(req).await.unwrap()) }
                    });
                    if alpn.as_deref() == Some(b"h2") {
                        let _ = http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                            .serve_connection(io, service)
                            .await;
                    } else {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, service)
                            .await;
                    }
                });
            }
            Err(e) => {
                tracing::error!("Accept error: {e}");
            }
        }
    }
}

fn generate_self_signed_cert() -> (
    Vec<quinn::rustls::pki_types::CertificateDer<'static>>,
    quinn::rustls::pki_types::PrivateKeyDer<'static>,
) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "filestore".into()])
            .expect("Failed to generate self-signed cert");
    (
        vec![quinn::rustls::pki_types::CertificateDer::from(cert.der().to_vec())],
        quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(
            quinn::rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
        ),
    )
}

fn load_certs_from_files(
    cert_path: &str,
    key_path: &str,
) -> (
    Vec<quinn::rustls::pki_types::CertificateDer<'static>>,
    quinn::rustls::pki_types::PrivateKeyDer<'static>,
) {
    let cert_file = std::fs::File::open(cert_path).expect("Failed to open cert file");
    let key_file = std::fs::File::open(key_path).expect("Failed to open key file");
    let certs: Vec<quinn::rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_file))
            .collect::<Result<Vec<_>, _>>()
            .expect("Failed to parse certs");
    let mut keys: Vec<_> = rustls_pemfile::pkcs8_private_keys(&mut BufReader::new(key_file))
        .collect::<Result<Vec<_>, _>>()
        .expect("Failed to parse keys");
    if keys.is_empty() {
        panic!("No private key found in {key_path}");
    }
    (
        certs,
        quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(keys.remove(0)),
    )
}

fn build_quic_server_config(
    certs: Vec<quinn::rustls::pki_types::CertificateDer<'static>>,
    key: quinn::rustls::pki_types::PrivateKeyDer<'static>,
) -> quinn::ServerConfig {
    let mut server_crypto = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("Failed to build QUIC TLS config");
    server_crypto.alpn_protocols = vec![b"h3".to_vec()];

    let quic_server_config =
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
            .expect("Failed to create QUIC server config");
    quinn::ServerConfig::with_crypto(Arc::new(quic_server_config))
}

async fn serve_h3(endpoint: quinn::Endpoint, app: Router) {
    tracing::info!("HTTP/3 (QUIC) listening on {}", endpoint.local_addr().unwrap());

    while let Some(incoming) = endpoint.accept().await {
        let app = app.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    tracing::debug!("H3 connection from {}", conn.remote_address());
                    if let Err(e) = handle_h3_connection(conn, app).await {
                        tracing::error!("H3 connection error: {}", e);
                    }
                }
                Err(e) => tracing::error!("QUIC accept error: {}", e),
            }
        });
    }
}

async fn handle_h3_connection(
    conn: quinn::Connection,
    app: Router,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut h3_conn: h3::server::Connection<h3_quinn::Connection, bytes::Bytes> =
        h3::server::Connection::new(h3_quinn::Connection::new(conn)).await?;

    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let app = app.clone();
                tokio::spawn(async move {
                    match resolver.resolve_request().await {
                        Ok((req, stream)) => {
                            if let Err(e) = handle_h3_request(app, req, stream).await {
                                tracing::error!("H3 request error: {}", e);
                            }
                        }
                        Err(e) => tracing::error!("H3 resolve error: {}", e),
                    }
                });
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("H3 connection closed: {}", e);
                break;
            }
        }
    }

    Ok(())
}

async fn handle_h3_request(
    app: Router,
    req: axum::http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<bytes::Bytes>, bytes::Bytes>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut body_data = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        body_data.extend_from_slice(chunk.chunk());
    }

    let (parts, _) = req.into_parts();
    let axum_req = axum::http::Request::from_parts(parts, Body::from(body_data));

    let response: Response = app.oneshot(axum_req).await.map_err(|e| -> Box<dyn std::error::Error> { format!("Router error: {}", e).into() })?;

    let (parts, body) = response.into_parts();
    let h3_resp = axum::http::Response::from_parts(parts, ());
    stream.send_response(h3_resp).await?;

    let body_bytes = axum::body::to_bytes(body, 100 * 1024 * 1024).await?;
    if !body_bytes.is_empty() {
        stream.send_data(bytes::Bytes::copy_from_slice(&body_bytes)).await?;
    }

    stream.finish().await?;
    Ok(())
}

fn resolve_path(state: &AppState, path: &str) -> Result<PathBuf, StatusCode> {
    let cleaned = path.trim_start_matches('/');
    if cleaned.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (ns_name, rest) = cleaned.split_once('/').unwrap_or((cleaned, ""));
    let ns_path = state.namespaces.get(ns_name).ok_or(StatusCode::NOT_FOUND)?;

    Ok(ns_path.join(rest))
}

fn guess_mime(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        _ => "application/octet-stream",
    }
}

async fn get_or_list(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let file_path = resolve_path(&state, &path)?;

    if file_path.is_dir() {
        return list_files_in_dir(&path, &file_path).await;
    }

    match tokio::fs::read(&file_path).await {
        Ok(bytes) => {
            let mime = guess_mime(&file_path);
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(bytes))
                .unwrap())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn put_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(path): Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let file_path = resolve_path(&state, &path)?;

    if let Some(parent) = file_path.parent() {
        if let Err(_) = tokio::fs::create_dir_all(parent).await {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    match tokio::fs::write(&file_path, &body).await {
        Ok(_) => Ok(StatusCode::CREATED),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn delete_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let file_path = resolve_path(&state, &path)?;

    match tokio::fs::remove_file(&file_path).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Deserialize)]
struct RenameRequest {
    destination: String,
}

async fn patch_rename(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(path): Path<String>,
    axum::Json(body): axum::Json<RenameRequest>,
) -> impl IntoResponse {
    let old_path = resolve_path(&state, &path)?;

    let cleaned = path.trim_start_matches('/');
    let (ns_name, _) = cleaned.split_once('/').unwrap_or((cleaned, ""));

    if body.destination.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let ns_path = state.namespaces.get(ns_name).ok_or(StatusCode::NOT_FOUND)?;
    let new_path = ns_path.join(&body.destination);

    if !old_path.is_dir() {
        return Err(StatusCode::NOT_FOUND);
    }
    if new_path.exists() {
        return Err(StatusCode::CONFLICT);
    }

    if let Some(parent) = new_path.parent() {
        if let Err(_) = tokio::fs::create_dir_all(parent).await {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    match tokio::fs::rename(&old_path, &new_path).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Serialize)]
struct ListResponse {
    files: Vec<String>,
}

async fn list_files_in_dir(prefix: &str, dir_path: &std::path::Path) -> Result<Response, StatusCode> {
    let mut files = Vec::new();

    let mut read_dir = match tokio::fs::read_dir(dir_path).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(axum::Json(ListResponse { files }).into_response());
        }
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        if let Ok(ft) = entry.file_type().await {
            if ft.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    let cleaned_prefix = prefix.trim_start_matches('/');
                    files.push(format!("{cleaned_prefix}/{name}"));
                }
            }
        }
    }

    files.sort();
    Ok(axum::Json(ListResponse { files }).into_response())
}
