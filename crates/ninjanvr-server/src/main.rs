use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as AutoConnBuilder;
use ninjanvr_config::DEFAULT_CONFIG_PATH;
use ninjanvr_server::{
    build_router_with_auth, generate_self_signed_tls_config,
    generate_self_signed_tls_config_with_sans, new_app_state,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::{error, info, warn};

pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const HTTP1_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
pub const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
pub const MAX_CONCURRENT_CONNECTIONS: usize = 1024;

/// Strips any client-supplied `x-peer-ip` headers (plus spoofing variants) and injects the
/// verified TCP socket peer IP so downstream rate limiters, proxy auth, and audit logs
/// never trust a client-controlled `x-peer-ip` header on either HTTP (`:5000`) or HTTPS (`:8971`).
pub fn sanitize_peer_ip_and_host_headers<B>(req: &mut Request<B>, remote_addr: SocketAddr) {
    while req.headers_mut().remove("x-peer-ip").is_some() {}
    let peer_ip_str = remote_addr.ip().to_string();
    if let Ok(val) = hyper::header::HeaderValue::from_str(&peer_ip_str) {
        req.headers_mut().insert("x-peer-ip", val);
    }
    if !req.headers().contains_key(hyper::header::HOST)
        && let Some(auth) = req.uri().authority()
        && let Ok(val) = hyper::header::HeaderValue::from_str(auth.as_str())
    {
        req.headers_mut().insert(hyper::header::HOST, val);
    }
}

/// Completes a TLS handshake within `timeout_dur`, aborting stalled/slowloris handshakes.
pub async fn accept_tls_with_timeout(
    tls_acceptor: &TlsAcceptor,
    tcp_stream: TcpStream,
    timeout_dur: Duration,
) -> Option<tokio_rustls::server::TlsStream<TcpStream>> {
    match tokio::time::timeout(timeout_dur, tls_acceptor.accept(tcp_stream)).await {
        Ok(Ok(tls_stream)) => Some(tls_stream),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Normalizes and filters configured `tls.san_hosts` entries so malformed URLs, ports,
/// or control characters in `config.yml` cannot crash TLS startup.
pub fn sanitize_san_hosts(raw_sans: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in raw_sans {
        let mut s = raw.trim();
        if let Some(rest) = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
        {
            s = rest;
        }
        if let Some((host_port, _)) = s.split_once('/') {
            s = host_port;
        }
        // Strip `:port` for bracketed IPv6 `[::1]:8971` or unbracketed IPv4/DNS `host:8971`
        if let Some(stripped_v6) = s.strip_prefix('[') {
            if let Some((v6_host, _)) = stripped_v6.split_once(']') {
                s = v6_host;
            }
        } else if s.matches(':').count() == 1
            && let Some((host, port)) = s.split_once(':')
            && port.chars().all(|c| c.is_ascii_digit())
        {
            s = host;
        }
        let s = s.trim();
        if s.is_empty() || s.len() > 253 {
            continue;
        }
        let valid_chars = s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':' || c == '*');
        if valid_chars
            && !out
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(s))
        {
            out.push(s.to_string());
        }
    }
    out
}

/// Validates that a TLS private key file on disk is a regular file (not a symlink) and
/// enforces `0600` owner-only permissions on Unix platforms.
pub fn ensure_private_key_permissions(key_path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(key_path)
        .with_context(|| format!("failed to stat TLS private key {}", key_path.display()))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!(
            "refusing to use symlinked TLS private key at {}",
            key_path.display()
        );
    }
    if !meta.file_type().is_file() {
        anyhow::bail!(
            "TLS private key path {} is not a regular file",
            key_path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(key_path, perms).with_context(|| {
                format!(
                    "failed to enforce 0600 permissions on TLS private key {}",
                    key_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Validates that a configuration path is not a directory or dangling/unsafe path.
pub fn validate_config_file_path(config_path: &Path) -> Result<()> {
    if config_path.as_os_str().is_empty() {
        anyhow::bail!("configuration file path must not be empty");
    }
    if let Ok(meta) = std::fs::symlink_metadata(config_path) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "refusing to load configuration through symlink {}",
                config_path.display()
            );
        }
        if meta.is_dir() {
            anyhow::bail!(
                "configuration path {} is a directory, expected a YAML file",
                config_path.display()
            );
        }
    }
    Ok(())
}

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
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&p| p > 0)
        .unwrap_or(5000);
    let mut https_port: u16 = std::env::var("NINJANVR_HTTPS_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&p| p > 0)
        .unwrap_or(8971);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(v) = args.next()
                    && !v.trim().is_empty()
                {
                    config_path = PathBuf::from(v);
                }
            }
            "--web-dir" => {
                if let Some(v) = args.next()
                    && !v.trim().is_empty()
                {
                    web_dir = PathBuf::from(v);
                }
            }
            "--media-dir" => {
                if let Some(v) = args.next()
                    && !v.trim().is_empty()
                {
                    media_dir = PathBuf::from(v);
                }
            }
            "--http-port" => {
                if let Some(v) = args.next()
                    && let Ok(p) = v.parse::<u16>()
                    && p > 0
                {
                    http_port = p;
                }
            }
            "--https-port" => {
                if let Some(v) = args.next()
                    && let Ok(p) = v.parse::<u16>()
                    && p > 0
                {
                    https_port = p;
                }
            }
            "--validate-config" => {
                validate_config_file_path(&config_path)?;
                ninjanvr_config::NinjaConfig::load(&config_path)?;
                println!("Config is valid: {}", config_path.display());
                return Ok(());
            }
            _ => {}
        }
    }

    if http_port == https_port {
        anyhow::bail!("HTTP port ({http_port}) and HTTPS port ({https_port}) must be distinct");
    }
    validate_config_file_path(&config_path)?;

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
    let raw_san_hosts = state.config.read().await.tls.san_hosts.clone();
    let san_hosts = sanitize_san_hosts(&raw_san_hosts);
    let app_http = build_router_with_auth(state.clone(), false);
    let app_https = build_router_with_auth(state, true);

    // 1. Spawn HTTP server on :5000 (internal / healthcheck port) with strict x-peer-ip sanitization
    let http_addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    let http_listener = TcpListener::bind(http_addr).await?;
    let http_conn_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    info!("Listening on HTTP http://{http_addr}");
    tokio::spawn(async move {
        loop {
            let (tcp_stream, remote_addr) = match http_listener.accept().await {
                Ok(conn) => conn,
                Err(err) => {
                    error!("HTTP accept error: {err:#}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    continue;
                }
            };
            let Ok(permit) = http_conn_sem.clone().try_acquire_owned() else {
                continue;
            };
            let _ = tcp_stream.set_nodelay(true);
            let app_clone = app_http.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let io = TokioIo::new(tcp_stream);
                let hyper_service =
                    hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                        sanitize_peer_ip_and_host_headers(&mut req, remote_addr);
                        let mut svc = app_clone.clone();
                        async move { svc.call(req).await }
                    });
                let mut builder = AutoConnBuilder::new(TokioExecutor::new());
                builder
                    .http1()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HTTP1_HEADER_READ_TIMEOUT);
                let _ = builder
                    .serve_connection_with_upgrades(io, hyper_service)
                    .await;
            });
        }
    });

    // 2. Run HTTPS server on :8971 (external authenticated TLS port)
    let https_addr = SocketAddr::from(([0, 0, 0, 0], https_port));
    let https_listener = TcpListener::bind(https_addr).await?;
    let tls_config = match generate_self_signed_tls_config_with_sans(&san_hosts) {
        Ok(cfg) => cfg,
        Err(err) => {
            warn!(
                "Failed to build TLS certificate with custom SANs ({err:#}); falling back to default SANs"
            );
            generate_self_signed_tls_config()?
        }
    };
    let tls_acceptor = TlsAcceptor::from(tls_config);
    let https_conn_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    info!("Listening on HTTPS https://{https_addr}");

    loop {
        let (tcp_stream, remote_addr) = match https_listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                error!("HTTPS accept error: {err:#}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };

        let Ok(permit) = https_conn_sem.clone().try_acquire_owned() else {
            continue;
        };
        let _ = tcp_stream.set_nodelay(true);
        let tls_acceptor = tls_acceptor.clone();
        let app_clone = app_https.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Some(tls_stream) =
                accept_tls_with_timeout(&tls_acceptor, tcp_stream, TLS_HANDSHAKE_TIMEOUT).await
            else {
                return;
            };
            let io = TokioIo::new(tls_stream);
            let hyper_service =
                hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                    sanitize_peer_ip_and_host_headers(&mut req, remote_addr);
                    let mut svc = app_clone.clone();
                    async move { svc.call(req).await }
                });
            let mut builder = AutoConnBuilder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(HTTP1_HEADER_READ_TIMEOUT);
            let _ = builder
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_peer_ip_and_host_headers_overwrites_spoofed_peer_ip_and_populates_host() {
        let mut req = Request::builder()
            .uri("https://nvr.local:8971/api/version")
            .header("x-peer-ip", "127.0.0.1")
            .header("X-Peer-Ip", "10.0.0.1")
            .body(())
            .unwrap();
        let remote: SocketAddr = "203.0.113.45:54321".parse().unwrap();
        sanitize_peer_ip_and_host_headers(&mut req, remote);

        let all_peer_ips: Vec<&str> = req
            .headers()
            .get_all("x-peer-ip")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(all_peer_ips, vec!["203.0.113.45"]);
        assert_eq!(
            req.headers()
                .get(hyper::header::HOST)
                .and_then(|v| v.to_str().ok()),
            Some("nvr.local:8971")
        );
    }

    #[tokio::test]
    async fn accept_tls_with_timeout_aborts_stalled_slowloris_handshake() {
        let tls_cfg = generate_self_signed_tls_config().unwrap();
        let acceptor = TlsAcceptor::from(tls_cfg);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Client opens TCP connection and sends zero TLS ClientHello bytes
        let _stalled_client = TcpStream::connect(addr).await.unwrap();
        let (server_tcp, _) = listener.accept().await.unwrap();

        let started = std::time::Instant::now();
        let res = accept_tls_with_timeout(&acceptor, server_tcp, Duration::from_millis(80)).await;
        assert!(
            res.is_none(),
            "stalled TLS handshake must time out and return None"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn sanitize_san_hosts_strips_schemes_ports_paths_and_rejects_invalid_entries() {
        let raw = vec![
            "https://nvr.example.com:8971/login".to_string(),
            "http://192.168.1.50:5000".to_string(),
            "[fd00::1]:8971".to_string(),
            "bad host with spaces".to_string(),
            "nvr.example.com".to_string(),
        ];
        let cleaned = sanitize_san_hosts(&raw);
        assert_eq!(
            cleaned,
            vec![
                "nvr.example.com".to_string(),
                "192.168.1.50".to_string(),
                "fd00::1".to_string(),
            ]
        );
        assert!(generate_self_signed_tls_config_with_sans(&cleaned).is_ok());
    }

    #[test]
    fn ensure_private_key_permissions_enforces_0600_and_rejects_symlinks() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-tls-perm-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let key_path = tmp_dir.join("tls.key");
        std::fs::write(&key_path, b"PRIVATE_KEY_TEST").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&key_path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&key_path, perms).unwrap();
            ensure_private_key_permissions(&key_path).unwrap();
            let new_mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(new_mode, 0o600);

            let link_path = tmp_dir.join("tls-link.key");
            std::os::unix::fs::symlink(&key_path, &link_path).unwrap();
            assert!(
                ensure_private_key_permissions(&link_path).is_err(),
                "symlinked private key must be rejected"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
