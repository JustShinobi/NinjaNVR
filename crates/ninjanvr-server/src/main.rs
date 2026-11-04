use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoConnBuilder;
use ninjanvr_config::DEFAULT_CONFIG_PATH;
use ninjanvr_server::{
    build_router_with_auth, generate_self_signed_tls_config_with_sans, new_app_state,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::{error, info};

#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut config_path = PathBuf::from(
        std::env::var("CONFIG_FILE").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string()),
    );
    let mut web_dir = PathBuf::from(
        std::env::var("NINJANVR_WEB_V2_DIR")
            .or_else(|_| std::env::var("NINJANVR_WEB_DIR"))
            .unwrap_or_else(|_| "/opt/ninjanvr/web-v2".to_string()),
    );
    let mut media_dir = PathBuf::from(
        std::env::var("NINJANVR_MEDIA_DIR").unwrap_or_else(|_| "/media/ninjanvr".to_string()),
    );
    let mut http_port: u16 = std::env::var("NINJANVR_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    let mut https_port: u16 = std::env::var("NINJANVR_HTTPS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8971);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(v) = args.next() {
                    config_path = PathBuf::from(v);
                }
            }
            "--web-dir" => {
                if let Some(v) = args.next() {
                    web_dir = PathBuf::from(v);
                }
            }
            "--media-dir" => {
                if let Some(v) = args.next() {
                    media_dir = PathBuf::from(v);
                }
            }
            "--http-port" => {
                if let Some(v) = args.next()
                    && let Ok(p) = v.parse()
                {
                    http_port = p;
                }
            }
            "--https-port" => {
                if let Some(v) = args.next()
                    && let Ok(p) = v.parse()
                {
                    https_port = p;
                }
            }
            "--validate-config" => {
                ninjanvr_config::NinjaConfig::load(&config_path)?;
                println!("Config is valid: {}", config_path.display());
                return Ok(());
            }
            _ => {}
        }
    }

    if !web_dir.exists() {
        let fallback_dist = PathBuf::from("web-v2/dist");
        let fallback = PathBuf::from("web-v2");
        if fallback_dist.exists() {
            web_dir = fallback_dist;
        } else if fallback.exists() {
            web_dir = fallback;
        }
    }

    let spawn_go2rtc = std::env::var("NINJANVR_DISABLE_GO2RTC")
        .map(|v| v != "1")
        .unwrap_or(true);

    info!(
        "Starting NinjaNVR (config={}, web_dir={}, media_dir={})",
        config_path.display(),
        web_dir.display(),
        media_dir.display()
    );

    let state = new_app_state(&config_path, web_dir, media_dir, spawn_go2rtc).await?;
    let san_hosts = state.config.read().await.tls.san_hosts.clone();
    let app_http = build_router_with_auth(state.clone(), false);
    let app_https = build_router_with_auth(state, true);

    // 1. Spawn HTTP server on :5000 (internal / healthcheck port)
    let http_addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    let http_listener = TcpListener::bind(http_addr).await?;
    info!("Listening on HTTP http://{http_addr}");
    tokio::spawn(async move {
        if let Err(err) = axum::serve(http_listener, app_http).await {
            error!("HTTP server error: {err:#}");
        }
    });

    // 2. Run HTTPS server on :8971 (external authenticated TLS port)
    let https_addr = SocketAddr::from(([0, 0, 0, 0], https_port));
    let https_listener = TcpListener::bind(https_addr).await?;
    let tls_config = generate_self_signed_tls_config_with_sans(&san_hosts)?;
    let tls_acceptor = TlsAcceptor::from(tls_config);
    info!("Listening on HTTPS https://{https_addr}");

    loop {
        let (tcp_stream, remote_addr) = match https_listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                error!("HTTPS accept error: {err:#}");
                continue;
            }
        };

        let tls_acceptor = tls_acceptor.clone();
        let app_clone = app_https.clone();
        tokio::spawn(async move {
            let Ok(tls_stream) = tls_acceptor.accept(tcp_stream).await else {
                return;
            };
            let io = TokioIo::new(tls_stream);
            let hyper_service =
                hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                    if !req.headers().contains_key(hyper::header::HOST)
                        && let Some(auth) = req.uri().authority()
                        && let Ok(val) = hyper::header::HeaderValue::from_str(auth.as_str())
                    {
                        req.headers_mut().insert(hyper::header::HOST, val);
                    }
                    let mut svc = app_clone.clone();
                    async move { svc.call(req).await }
                });
            let _ = AutoConnBuilder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
            let _ = remote_addr;
        });
    }
}
