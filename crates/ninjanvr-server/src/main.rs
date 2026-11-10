#![forbid(unsafe_code)]

use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as AutoConnBuilder;
use ninjanvr_config::{DEFAULT_CONFIG_PATH, DEFAULT_GO2RTC_SHM_PATH};
use ninjanvr_server::{
    build_router_with_auth, generate_self_signed_tls_config,
    generate_self_signed_tls_config_with_sans, new_app_state,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::{error, info, warn};

pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const HTTP1_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
pub const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
pub const MAX_CONCURRENT_CONNECTIONS: usize = 1024;
pub const HTTP2_MAX_CONCURRENT_STREAMS: u32 = 128;
pub const HTTP2_MAX_HEADER_LIST_SIZE: u32 = 64 * 1024;
pub const MAX_PRIVATE_KEY_FILE_BYTES: u64 = 256 * 1024;

/// Strips any client-supplied `x-peer-ip` headers (plus spoofing variants) and injects the
/// verified TCP socket peer IP (canonicalizing IPv4-mapped IPv6 addresses) so downstream rate limiters,
/// proxy auth, and audit logs never trust a client-controlled `x-peer-ip` header on either HTTP (`:5000`) or HTTPS (`:8971`).
pub fn sanitize_peer_ip_and_host_headers<B>(req: &mut Request<B>, remote_addr: SocketAddr) {
    while req.headers_mut().remove("x-peer-ip").is_some() {}
    let peer_ip_str = remote_addr.ip().to_canonical().to_string();
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

fn is_valid_san_dns_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    let body = if let Some(rest) = s.strip_prefix("*.") {
        if rest.is_empty() || !rest.contains('.') {
            return false;
        }
        rest
    } else {
        s
    };
    if body.contains('*') || body.starts_with('.') || body.ends_with('.') {
        return false;
    }
    for label in body.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return false;
        }
    }
    true
}

/// Normalizes and filters configured `tls.san_hosts` entries so malformed URLs, ports,
/// or control characters in `config.yml` cannot crash TLS startup or poison valid SAN entries.
pub fn sanitize_san_hosts(raw_sans: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in raw_sans {
        let mut s = raw.trim();
        if s.as_bytes()
            .get(..8)
            .is_some_and(|b| b.eq_ignore_ascii_case(b"https://"))
        {
            s = &s[8..];
        } else if s
            .as_bytes()
            .get(..7)
            .is_some_and(|b| b.eq_ignore_ascii_case(b"http://"))
        {
            s = &s[7..];
        }
        if let Some((before_delim, _)) = s.split_once(['/', '?', '#']) {
            s = before_delim;
        }
        if s.contains('@') || s.contains('\\') {
            continue;
        }
        // Strip `:port` for bracketed IPv6 `[::1]:8971` or unbracketed IPv4/DNS `host:8971`
        if let Some(stripped_v6) = s.strip_prefix('[') {
            let Some((v6_host, port_rest)) = stripped_v6.split_once(']') else {
                continue;
            };
            if v6_host.parse::<std::net::Ipv6Addr>().is_err() {
                continue;
            }
            if !port_rest.is_empty() {
                let Some(port_str) = port_rest.strip_prefix(':') else {
                    continue;
                };
                if !port_str.parse::<u16>().is_ok_and(|p| p > 0) {
                    continue;
                }
            }
            s = v6_host;
        } else if s.matches(':').count() == 1 {
            let Some((host, port)) = s.split_once(':') else {
                continue;
            };
            if !port.parse::<u16>().is_ok_and(|p| p > 0) {
                continue;
            }
            s = host;
        }
        let s = s.trim();
        if s.is_empty() || s.len() > 253 {
            continue;
        }
        let is_valid = s.parse::<IpAddr>().is_ok() || is_valid_san_dns_name(s);
        if is_valid && !out.iter().any(|existing| existing.eq_ignore_ascii_case(s)) {
            out.push(s.to_string());
        }
    }
    out
}

/// Validates that a TLS private key file on disk is a non-empty regular file (not a symlink or parent traversal)
/// and enforces `0600` owner-only permissions on Unix platforms.
pub fn ensure_private_key_permissions(key_path: &Path) -> Result<()> {
    if key_path.as_os_str().is_empty()
        || key_path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        anyhow::bail!("unsafe TLS private key path {}", key_path.display());
    }
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
    if meta.len() == 0 || meta.len() > MAX_PRIVATE_KEY_FILE_BYTES {
        anyhow::bail!(
            "TLS private key {} has invalid file size ({} bytes)",
            key_path.display(),
            meta.len()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(key_path)
            .with_context(|| {
                format!(
                    "failed to open TLS private key handle {}",
                    key_path.display()
                )
            })?;
        let f_meta = file.metadata().with_context(|| {
            format!(
                "failed to fstat TLS private key handle {}",
                key_path.display()
            )
        })?;
        if !f_meta.file_type().is_file() || (f_meta.dev(), f_meta.ino()) != (meta.dev(), meta.ino())
        {
            anyhow::bail!(
                "refusing to use TLS private key whose inode changed or is not a regular file at {}",
                key_path.display()
            );
        }
        let mode = f_meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            let mut perms = f_meta.permissions();
            perms.set_mode(0o600);
            file.set_permissions(perms).with_context(|| {
                format!(
                    "failed to enforce 0600 permissions on TLS private key {}",
                    key_path.display()
                )
            })?;
        }
    }
    Ok(())
}

pub const MAX_CONFIG_FILE_BYTES: u64 = 1_048_576;

/// Validates that a configuration path is not empty, does not traverse `..`, and is a regular file when present.
pub fn validate_config_file_path(config_path: &Path) -> Result<()> {
    if config_path.as_os_str().is_empty() {
        anyhow::bail!("configuration file path must not be empty");
    }
    if config_path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        anyhow::bail!(
            "refusing configuration path with parent directory traversal: {}",
            config_path.display()
        );
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
        if !meta.file_type().is_file() {
            anyhow::bail!(
                "configuration path {} is not a regular file",
                config_path.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(config_path)
                .with_context(|| {
                    format!(
                        "failed to open configuration file {} with O_NOFOLLOW",
                        config_path.display()
                    )
                })?;
            let f_meta = file.metadata().with_context(|| {
                format!(
                    "failed to fstat configuration file {}",
                    config_path.display()
                )
            })?;
            if !f_meta.file_type().is_file()
                || (f_meta.dev(), f_meta.ino()) != (meta.dev(), meta.ino())
            {
                anyhow::bail!(
                    "refusing configuration file whose inode changed or is not a regular file at {}",
                    config_path.display()
                );
            }
            if f_meta.len() > MAX_CONFIG_FILE_BYTES {
                anyhow::bail!(
                    "configuration file {} exceeds maximum allowed size ({} bytes)",
                    config_path.display(),
                    MAX_CONFIG_FILE_BYTES
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCliArgs {
    pub config_path: PathBuf,
    pub web_dir: PathBuf,
    pub media_dir: PathBuf,
    pub http_port: u16,
    pub https_port: u16,
    pub validate_only: bool,
}

/// Parses CLI flags (`--config`, `--web-dir`, `--media-dir`, `--http-port`, `--https-port`, `--validate-config`)
/// independently of argument ordering so `--validate-config --config <path>` always validates `<path>`.
pub fn parse_cli_args<I>(
    raw_args: I,
    mut config_path: PathBuf,
    mut web_dir: PathBuf,
    mut media_dir: PathBuf,
    mut http_port: u16,
    mut https_port: u16,
) -> ParsedCliArgs
where
    I: IntoIterator<Item = String>,
{
    let mut validate_only = false;
    let mut args = raw_args.into_iter().peekable();
    while let Some(arg) = args.next() {
        let trimmed_arg = arg.trim();
        if let Some(v) = trimmed_arg.strip_prefix("--config=") {
            if !v.trim().is_empty() && !v.trim().starts_with("--") {
                config_path = PathBuf::from(v.trim());
            }
            continue;
        }
        if let Some(v) = trimmed_arg.strip_prefix("--web-dir=") {
            if !v.trim().is_empty() && !v.trim().starts_with("--") {
                web_dir = PathBuf::from(v.trim());
            }
            continue;
        }
        if let Some(v) = trimmed_arg.strip_prefix("--media-dir=") {
            if !v.trim().is_empty() && !v.trim().starts_with("--") {
                media_dir = PathBuf::from(v.trim());
            }
            continue;
        }
        if let Some(v) = trimmed_arg.strip_prefix("--http-port=") {
            if let Ok(p) = v.trim().parse::<u16>()
                && p > 0
            {
                http_port = p;
            }
            continue;
        }
        if let Some(v) = trimmed_arg.strip_prefix("--https-port=") {
            if let Ok(p) = v.trim().parse::<u16>()
                && p > 0
            {
                https_port = p;
            }
            continue;
        }
        match trimmed_arg {
            "--config" => {
                if let Some(v) = args.next_if(|next| !next.trim().starts_with("--"))
                    && !v.trim().is_empty()
                {
                    config_path = PathBuf::from(v.trim());
                }
            }
            "--web-dir" => {
                if let Some(v) = args.next_if(|next| !next.trim().starts_with("--"))
                    && !v.trim().is_empty()
                {
                    web_dir = PathBuf::from(v.trim());
                }
            }
            "--media-dir" => {
                if let Some(v) = args.next_if(|next| !next.trim().starts_with("--"))
                    && !v.trim().is_empty()
                {
                    media_dir = PathBuf::from(v.trim());
                }
            }
            "--http-port" => {
                if let Some(v) = args.next_if(|next| !next.trim().starts_with("--"))
                    && let Ok(p) = v.trim().parse::<u16>()
                    && p > 0
                {
                    http_port = p;
                }
            }
            "--https-port" => {
                if let Some(v) = args.next_if(|next| !next.trim().starts_with("--"))
                    && let Ok(p) = v.trim().parse::<u16>()
                    && p > 0
                {
                    https_port = p;
                }
            }
            "--validate-config" => {
                validate_only = true;
            }
            _ => {}
        }
    }
    ParsedCliArgs {
        config_path,
        web_dir,
        media_dir,
        http_port,
        https_port,
        validate_only,
    }
}

/// Terminates supervised child processes (`go2rtc` and camera decoder `ffmpeg` PIDs) and removes
/// the ephemeral `/dev/shm/go2rtc.yaml` file containing unredacted camera credentials on shutdown.
pub fn cleanup_supervised_processes_and_shm(
    go2rtc_pid: u32,
    camera_pids: &[u32],
    shm_config_path: &Path,
) {
    let self_pid = std::process::id();
    for &pid in std::iter::once(&go2rtc_pid).chain(camera_pids.iter()) {
        if pid > 1 && pid != self_pid {
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
    }
    if let Ok(meta) = std::fs::symlink_metadata(shm_config_path)
        && meta.file_type().is_file()
        && !meta.file_type().is_symlink()
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(file) = std::fs::OpenOptions::new()
                .write(true)
                .open(shm_config_path)
                && let Ok(f_meta) = file.metadata()
                && f_meta.file_type().is_file()
                && (f_meta.dev(), f_meta.ino()) == (meta.dev(), meta.ino())
            {
                let _ = file.set_len(0);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::write(shm_config_path, b"");
        }
        let _ = std::fs::remove_file(shm_config_path);
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, initiating graceful shutdown");
            }
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                info!("Received SIGTERM, initiating graceful shutdown");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("Received SIGINT, initiating graceful shutdown");
    }
}

#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let default_config_path = PathBuf::from(
        std::env::var("CONFIG_FILE").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string()),
    );
    let default_web_dir = PathBuf::from(
        std::env::var("NINJANVR_WEB_DIR").unwrap_or_else(|_| "/opt/ninjanvr/web".to_string()),
    );
    let default_media_dir = PathBuf::from(
        std::env::var("NINJANVR_MEDIA_DIR").unwrap_or_else(|_| "/media/ninjanvr".to_string()),
    );
    let default_http_port: u16 = std::env::var("NINJANVR_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&p| p > 0)
        .unwrap_or(5000);
    let default_https_port: u16 = std::env::var("NINJANVR_HTTPS_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&p| p > 0)
        .unwrap_or(8971);

    let parsed = parse_cli_args(
        std::env::args().skip(1),
        default_config_path,
        default_web_dir,
        default_media_dir,
        default_http_port,
        default_https_port,
    );
    let config_path = parsed.config_path;
    let mut web_dir = parsed.web_dir;
    let media_dir = parsed.media_dir;
    let http_port = parsed.http_port;
    let https_port = parsed.https_port;

    validate_config_file_path(&config_path)?;
    if parsed.validate_only {
        ninjanvr_config::NinjaConfig::load(&config_path)?;
        println!("Config is valid: {}", config_path.display());
        return Ok(());
    }

    if http_port == https_port {
        anyhow::bail!("HTTP port ({http_port}) and HTTPS port ({https_port}) must be distinct");
    }

    if !web_dir.exists() {
        let fallback_dist = PathBuf::from("web/dist");
        let fallback = PathBuf::from("web");
        if fallback_dist.exists() {
            web_dir = fallback_dist;
        } else if fallback.exists() {
            web_dir = fallback;
        }
    }

    let spawn_go2rtc = !std::env::var("NINJANVR_DISABLE_GO2RTC").is_ok_and(|v| v == "1");

    info!(
        "Starting NinjaNVR (config={}, web_dir={}, media_dir={})",
        config_path.display(),
        web_dir.display(),
        media_dir.display()
    );

    let state = new_app_state(&config_path, web_dir, media_dir, spawn_go2rtc).await?;
    let (raw_san_hosts, mqtt_client_key) = {
        let cfg = state.config.read().await;
        (cfg.tls.san_hosts.clone(), cfg.mqtt.tls_client_key.clone())
    };
    if let Some(ref key_path) = mqtt_client_key
        && std::fs::symlink_metadata(key_path).is_ok()
        && let Err(err) = ensure_private_key_permissions(key_path)
    {
        warn!("MQTT TLS private key permission check warning: {err:#}");
    }
    let san_hosts = sanitize_san_hosts(&raw_san_hosts);
    let go2rtc_supervisor = state.go2rtc_supervisor.clone();
    let camera_metrics = state.camera_metrics.clone();
    let enforce_http_auth = std::env::var("NINJANVR_HTTP_ENFORCE_AUTH")
        .or_else(|_| std::env::var("NINJANVR_ENFORCE_HTTP_AUTH"))
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    let app_http = build_router_with_auth(state.clone(), enforce_http_auth);
    let app_https = build_router_with_auth(state, true);
    let (shutdown_tx, mut http_shutdown_rx) = watch::channel(false);

    // 1. Spawn HTTP server on :5000 (internal / healthcheck port) with strict x-peer-ip sanitization
    let http_addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    let http_listener = TcpListener::bind(http_addr).await?;
    let http_conn_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    info!("Listening on HTTP http://{http_addr}");
    tokio::spawn(async move {
        loop {
            if *http_shutdown_rx.borrow() {
                break;
            }
            let (tcp_stream, remote_addr) = tokio::select! {
                biased;
                _ = http_shutdown_rx.changed() => {
                    if *http_shutdown_rx.borrow() {
                        break;
                    }
                    continue;
                }
                res = http_listener.accept() => match res {
                    Ok(conn) => conn,
                    Err(err) => {
                        error!("HTTP accept error: {err:#}");
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    }
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
                builder
                    .http2()
                    .timer(TokioTimer::new())
                    .max_concurrent_streams(HTTP2_MAX_CONCURRENT_STREAMS)
                    .max_header_list_size(HTTP2_MAX_HEADER_LIST_SIZE);
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

    let shutdown_signal = wait_for_shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        let (tcp_stream, remote_addr) = tokio::select! {
            biased;
            _ = &mut shutdown_signal => {
                let _ = shutdown_tx.send(true);
                break;
            }
            res = https_listener.accept() => match res {
                Ok(conn) => conn,
                Err(err) => {
                    error!("HTTPS accept error: {err:#}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    continue;
                }
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
            builder
                .http2()
                .timer(TokioTimer::new())
                .max_concurrent_streams(HTTP2_MAX_CONCURRENT_STREAMS)
                .max_header_list_size(HTTP2_MAX_HEADER_LIST_SIZE);
            let _ = builder
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
        });
    }

    let cam_pids: Vec<u32> = camera_metrics
        .read()
        .await
        .values()
        .map(|m| m.pid)
        .collect();
    cleanup_supervised_processes_and_shm(
        go2rtc_supervisor.pid(),
        &cam_pids,
        Path::new(DEFAULT_GO2RTC_SHM_PATH),
    );
    Ok(())
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

    #[test]
    fn cycle2_main_sanitizes_malformed_sans_validates_cli_order_and_cleans_up_shm() {
        // 1. MAIN-C2-02a: Uppercase schemes, query/fragment suffixes, and wildcard SANs are normalized,
        //    while poisoned entries ("host:badport", "-bad.host", "bad..host", "*.*.example.com", "user@host")
        //    are filtered out so rcgen never fails on the remaining valid SAN list
        let raw_sans = vec![
            "HTTPS://cam.example.com:8971?foo=1#bar".to_string(),
            "*.sub.example.com".to_string(),
            "host:badport".to_string(),
            "-leading-hyphen.local".to_string(),
            "double..dot.local".to_string(),
            "*.*.example.com".to_string(),
            "*.tldonly".to_string(),
            "admin:pass@nvr.local".to_string(),
            "[2001:db8::5]:0".to_string(),
            "[2001:db8::5]:8971".to_string(),
        ];
        let cleaned = sanitize_san_hosts(&raw_sans);
        assert_eq!(
            cleaned,
            vec![
                "cam.example.com".to_string(),
                "*.sub.example.com".to_string(),
                "2001:db8::5".to_string(),
            ]
        );
        assert!(
            generate_self_signed_tls_config_with_sans(&cleaned).is_ok(),
            "cleaned SANs must always succeed in rcgen certificate generation"
        );

        // 2. MAIN-C2-01a: IPv4-mapped IPv6 peer addresses (::ffff:198.51.100.7) are canonicalized to IPv4
        let mut req = Request::builder().uri("/api/stats").body(()).unwrap();
        let mapped_peer: SocketAddr = "[::ffff:198.51.100.7]:44321".parse().unwrap();
        sanitize_peer_ip_and_host_headers(&mut req, mapped_peer);
        assert_eq!(
            req.headers().get("x-peer-ip").and_then(|v| v.to_str().ok()),
            Some("198.51.100.7")
        );

        // 3. MAIN-C2-02b: --validate-config before --config <path> must still honor the custom --config path
        let cli = parse_cli_args(
            vec![
                "--validate-config".to_string(),
                "--config".to_string(),
                "/custom/ninjanvr.yml".to_string(),
                "--http-port".to_string(),
                "5080".to_string(),
                "--https-port".to_string(),
                "8443".to_string(),
            ],
            PathBuf::from("/config/config.yml"),
            PathBuf::from("/opt/ninjanvr/web"),
            PathBuf::from("/media/ninjanvr"),
            5000,
            8971,
        );
        assert!(cli.validate_only);
        assert_eq!(cli.config_path, PathBuf::from("/custom/ninjanvr.yml"));
        assert_eq!(cli.http_port, 5080);
        assert_eq!(cli.https_port, 8443);

        // 4. MAIN-C2-02c: Empty private key file and parent-traversal paths must be rejected
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-main-c2-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let empty_key = tmp_dir.join("empty.key");
        std::fs::write(&empty_key, b"").unwrap();
        assert!(
            ensure_private_key_permissions(&empty_key).is_err(),
            "0-byte TLS private key must be rejected"
        );
        assert!(
            validate_config_file_path(Path::new("/config/../etc/passwd")).is_err(),
            "parent traversal in config path must be rejected"
        );
        #[cfg(unix)]
        {
            let real_cfg = tmp_dir.join("real.yml");
            std::fs::write(&real_cfg, b"cameras: {}\n").unwrap();
            assert!(validate_config_file_path(&real_cfg).is_ok());
            let sym_cfg = tmp_dir.join("sym.yml");
            std::os::unix::fs::symlink(&real_cfg, &sym_cfg).unwrap();
            assert!(
                validate_config_file_path(&sym_cfg).is_err(),
                "symlinked config file must be rejected"
            );
            let huge_cfg = tmp_dir.join("huge.yml");
            std::fs::write(&huge_cfg, vec![b'a'; (MAX_CONFIG_FILE_BYTES as usize) + 1]).unwrap();
            assert!(
                validate_config_file_path(&huge_cfg).is_err(),
                "oversized config file > 1 MiB must be rejected"
            );
        }

        // 5. MAIN-C2-01b: cleanup_supervised_processes_and_shm wipes and removes /dev/shm/go2rtc.yaml
        let shm_file = tmp_dir.join("go2rtc.yaml");
        std::fs::write(
            &shm_file,
            b"streams:\n  cam: rtsp://admin:secret@192.0.2.1/main\n",
        )
        .unwrap();
        cleanup_supervised_processes_and_shm(0, &[], &shm_file);
        assert!(
            !shm_file.exists(),
            "ephemeral go2rtc shm config must be removed on supervisor shutdown"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn cycle3_main_utf8_san_bounds_bracketed_non_ipv6_rejection_and_cli_flag_guard() {
        // 1. MAIN-C3-01: Multi-byte UTF-8 strings spanning byte indices 7 or 8 and bracketed non-IPv6
        //    hosts ("[nvr.example.com]:8971", "[127.0.0.1]:8971") must be filtered without panicking
        let raw_sans = vec![
            "123456\u{00e9}89".to_string(),
            "12345\u{00e9}789".to_string(),
            "\u{1f525}https://nvr.local".to_string(),
            "[nvr.example.com]:8971".to_string(),
            "[127.0.0.1]:8971".to_string(),
            "https://valid-nvr.example.com:8971/ui".to_string(),
        ];
        let cleaned = sanitize_san_hosts(&raw_sans);
        assert_eq!(cleaned, vec!["valid-nvr.example.com".to_string()]);

        // 2. MAIN-C3-02: "--config --validate-config" must not swallow "--validate-config" as the config path,
        //    and "--config=/path" equals-syntax must be parsed properly
        let cli_missing_val = parse_cli_args(
            vec!["--config".to_string(), "--validate-config".to_string()],
            PathBuf::from("/config/default.yml"),
            PathBuf::from("/opt/ninjanvr/web"),
            PathBuf::from("/media/ninjanvr"),
            5000,
            8971,
        );
        assert!(
            cli_missing_val.validate_only,
            "--validate-config after bare --config must still set validate_only = true"
        );
        assert_eq!(
            cli_missing_val.config_path,
            PathBuf::from("/config/default.yml"),
            "bare --config before another flag must retain default config_path"
        );

        let cli_eq = parse_cli_args(
            vec![
                "--config=/etc/ninjanvr/custom.yml".to_string(),
                "--http-port=5090".to_string(),
                "--https-port=8999".to_string(),
                "--validate-config".to_string(),
            ],
            PathBuf::from("/config/default.yml"),
            PathBuf::from("/opt/ninjanvr/web"),
            PathBuf::from("/media/ninjanvr"),
            5000,
            8971,
        );
        assert!(cli_eq.validate_only);
        assert_eq!(
            cli_eq.config_path,
            PathBuf::from("/etc/ninjanvr/custom.yml")
        );
        assert_eq!(cli_eq.http_port, 5090);
        assert_eq!(cli_eq.https_port, 8999);
    }

    #[tokio::test]
    async fn watch_shutdown_with_biased_select_terminates_accept_loop_immediately_when_concurrent_ready()
     {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Case 1: Shutdown signal fired BEFORE entering the accept loop while TCP clients are already queued
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let _queued_client1 = TcpStream::connect(addr).await.unwrap();
        let _queued_client2 = TcpStream::connect(addr).await.unwrap();
        shutdown_tx.send(true).unwrap();

        let mut accepted_before_break = 0usize;
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                res = listener.accept() => {
                    if res.is_ok() {
                        accepted_before_break += 1;
                    }
                }
            }
        }
        assert_eq!(
            accepted_before_break, 0,
            "watch-based shutdown signal fired before loop entry must terminate immediately without accepting queued connections"
        );

        // Case 2: Both `shutdown_rx.changed()` and `listener.accept()` are ready simultaneously inside `tokio::select! { biased; ... }`
        let (shutdown_tx2, mut shutdown_rx2) = watch::channel(false);
        // Mark initial false as seen so `shutdown_rx2.changed()` awaits the next transition
        let _ = *shutdown_rx2.borrow_and_update();
        let _queued_client3 = TcpStream::connect(addr).await.unwrap();
        shutdown_tx2.send(true).unwrap();

        let mut accepted_concurrent = 0usize;
        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx2.changed() => {
                    if *shutdown_rx2.borrow() {
                        break;
                    }
                }
                res = listener.accept() => {
                    if res.is_ok() {
                        accepted_concurrent += 1;
                    }
                }
            }
        }
        assert_eq!(
            accepted_concurrent, 0,
            "biased tokio::select! must prioritize ready shutdown_rx.changed() over ready listener.accept()"
        );
    }
}
