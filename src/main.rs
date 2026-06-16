//! cua-bridge — Native Rust MCP bridge for cua-driver
//!
//! A minimal streamableHttp MCP server that proxies requests to cua-driver
//! over stdio. Replaces supergateway (Node.js, ~200MB) with a single native
//! binary (~2MB, zero runtime deps).
//!
//! ## Architecture
//!
//! ```text
//! Client → POST /mcp (JSON-RPC)          → cua-bridge → cua-driver stdio
//! Client → GET  /mcp (SSE streamableHttp) → cua-bridge (broadcasts all responses)
//!        → GET  /health
//! ```
//!
//! SSE support on GET /mcp is the key feature that makes this work with
//! Hermes Agent's native MCP client (streamableHttp transport).
//!
//! ## Quick start
//!
//! ```bash
//! cargo build --release
//! ./target/release/cua-bridge \
//!   --cua-driver /Applications/CuaDriver.app/Contents/MacOS/cua-driver \
//!   --port 8080
//! ```

use axum::{
    extract::State,
    http::StatusCode,
    response::{sse::{Event, KeepAlive, Sse}, IntoResponse, Response},
    routing::get,
    Router,
};
use futures::stream::Stream;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

struct Bridge {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<Box<dyn Write + Send>>>,
    stdout: Mutex<Option<BufReader<Box<dyn Read + Send>>>>,
    tx: broadcast::Sender<String>,
    driver_path: String,
    driver_healthy: AtomicBool,
}

const MCP_TIMEOUT: Duration = Duration::from_secs(30);

fn read_mcp_msg(reader: &mut BufReader<impl Read>, timeout: Duration) -> anyhow::Result<String> {
    let start = std::time::Instant::now();
    let mut line = String::new();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("timeout after {:?}", timeout);
        }
        line.clear();
        reader.read_line(&mut line)?;
        if !line.trim().is_empty() {
            return Ok(line.trim().to_string());
        }
    }
}

fn send_mcp_msg(stdin: &mut (impl Write + ?Sized), msg: &str) -> anyhow::Result<()> {
    writeln!(stdin, "{}", msg)?;
    stdin.flush()?;
    Ok(())
}

impl Bridge {
    fn spawn_driver(driver_path: &str) -> anyhow::Result<(Child, Box<dyn Write + Send>, Box<dyn Read + Send>)> {
        tracing::info!("Starting cua-driver: {}", driver_path);
        let mut child = Command::new(driver_path)
            .arg("mcp").arg("--no-overlay").arg("--no-daemon-relaunch")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn()?;
        tracing::info!("cua-driver pid {}", child.id());
        let stdin = Box::new(child.stdin.take().unwrap());
        let stdout = Box::new(child.stdout.take().unwrap());
        Ok((child, stdin, stdout))
    }

    fn restart_driver(self: &Arc<Self>) {
        tracing::warn!("Restarting cua-driver...");
        self.driver_healthy.store(false, Ordering::SeqCst);

        let old_child = { self.child.lock().take() };
        if let Some(mut c) = old_child {
            let _ = c.kill();
            let _ = c.wait();
        }

        match Self::spawn_driver(&self.driver_path) {
            Ok((child, stdin, stdout)) => {
                *self.child.lock() = Some(child);
                *self.stdin.lock() = Some(stdin);
                *self.stdout.lock() = Some(BufReader::new(stdout));
                self.driver_healthy.store(true, Ordering::SeqCst);
                tracing::info!("cua-driver restarted successfully");
            }
            Err(e) => {
                tracing::error!("Failed to restart cua-driver: {}", e);
            }
        }
    }
}

async fn handle_get_mcp(
    State(bridge): State<Arc<Bridge>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = bridge.tx.subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("endpoint").data("/mcp"));
        loop {
            match rx.recv().await {
                Ok(msg) => yield Ok(Event::default().data(msg)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn handle_post_mcp(
    State(bridge): State<Arc<Bridge>>,
    body: String,
) -> Response {
    let request = body.trim().to_string();

    let response = {
        let mut stdin_opt = bridge.stdin.lock();
        let mut stdout_opt = bridge.stdout.lock();

        if stdin_opt.is_none() || stdout_opt.is_none() {
            tracing::warn!("cua-driver not running, restarting...");
            drop(stdin_opt);
            drop(stdout_opt);
            bridge.restart_driver();
        }

        let mut stdin_guard = bridge.stdin.lock();
        let mut stdout_guard = bridge.stdout.lock();

        let stdin = match stdin_guard.as_mut() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32603, "message": "cua-driver not running"},
                    "id": null
                });
                bridge.tx.send(err.to_string()).ok();
                return StatusCode::ACCEPTED.into_response();
            }
        };
        let stdout = match stdout_guard.as_mut() {
            Some(s) => s,
            None => {
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32603, "message": "cua-driver stdout not available"},
                    "id": null
                });
                bridge.tx.send(err.to_string()).ok();
                return StatusCode::ACCEPTED.into_response();
            }
        };

        if let Err(e) = send_mcp_msg(stdin, &request) {
            tracing::error!("bridge write error: {}", e);
            bridge.driver_healthy.store(false, Ordering::SeqCst);
            let err = serde_json::json!({
                "jsonrpc": "2.0",
                "error": {"code": -32603, "message": format!("bridge write: {}", e)},
                "id": null
            });
            bridge.tx.send(err.to_string()).ok();
            return StatusCode::ACCEPTED.into_response();
        }

        match read_mcp_msg(stdout, MCP_TIMEOUT) {
            Ok(resp) => {
                bridge.tx.send(resp.clone()).ok();
                resp
            }
            Err(e) => {
                tracing::error!("bridge read error (timeout/hang): {}", e);
                bridge.driver_healthy.store(false, Ordering::SeqCst);
                let err_str = format!("{}", e);
                if err_str.contains("timeout") {
                    drop(stdin_guard);
                    drop(stdout_guard);
                    bridge.restart_driver();
                }
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32603, "message": format!("bridge read: {}", err_str)},
                    "id": null
                });
                bridge.tx.send(err.to_string()).ok();
                return StatusCode::ACCEPTED.into_response();
            }
        }
    };

    (StatusCode::OK, [("content-type", "application/json")], response).into_response()
}

async fn handle_options() -> Response {
    (
        StatusCode::OK,
        [
            ("access-control-allow-origin", "*"),
            ("access-control-allow-methods", "GET, POST, OPTIONS"),
            ("access-control-allow-headers", "content-type, mcp-session-id"),
        ],
        "",
    ).into_response()
}

async fn handle_health(State(bridge): State<Arc<Bridge>>) -> impl IntoResponse {
    let healthy = bridge.driver_healthy.load(Ordering::SeqCst);
    let child_alive = bridge.child.lock().as_ref()
        .map(|c| matches!(c.try_wait(), Ok(None)))
        .unwrap_or(false);

    let status = if healthy && child_alive {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let body = serde_json::json!({
        "status": if healthy && child_alive { "ok" } else { "degraded" },
        "driver_healthy": healthy,
        "driver_alive": child_alive,
    });

    (status, [(axum::http::header::CONTENT_TYPE, "application/json")], body.to_string())
}

fn detect_tailscale_ip() -> Option<IpAddr> {
    Command::new("tailscale").args(["ip", "-4"]).output().ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let mut cua_driver = "/Applications/CuaDriver.app/Contents/MacOS/cua-driver".to_string();
    let mut port: u16 = 8080;
    let mut extra_bind: Option<IpAddr> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cua-driver" => { i += 1; cua_driver = args[i].clone(); }
            "--port" => { i += 1; port = args[i].parse()?; }
            "--tailscale-ip" => { i += 1; extra_bind = Some(args[i].parse()?); }
            _ => { eprintln!("Unknown flag: {}", args[i]); std::process::exit(1); }
        }
        i += 1;
    }

    let (child, child_stdin, child_stdout) = Bridge::spawn_driver(&cua_driver)?;

    let (tx, _) = broadcast::channel::<String>(64);
    let bridge = Arc::new(Bridge {
        child: Mutex::new(Some(child)),
        stdin: Mutex::new(Some(child_stdin)),
        stdout: Mutex::new(Some(BufReader::new(child_stdout))),
        tx,
        driver_path: cua_driver,
        driver_healthy: AtomicBool::new(true),
    });

    let app = Router::new()
        .route("/mcp", get(handle_get_mcp).post(handle_post_mcp).options(handle_options))
        .route("/health", get(handle_health))
        .with_state(bridge.clone());

    let tailscale_ip = extra_bind.or_else(detect_tailscale_ip);
    let mut addrs: Vec<SocketAddr> = Vec::new();
    if let Some(ts) = tailscale_ip { addrs.push(SocketAddr::new(ts, port)); }
    addrs.push(SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port));

    let mut listener = None;
    for addr in &addrs {
        match TcpListener::bind(*addr).await {
            Ok(l) => { tracing::info!("listening on http://{}", addr); listener = Some(l); break; }
            Err(e) => { tracing::warn!("could not bind {}: {}", addr, e); }
        }
    }
    let listener = listener.ok_or_else(|| anyhow::anyhow!("no bind address"))?;
    tracing::info!("cua-bridge ready");

    let bs = bridge.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let mut child_opt = bs.child.lock();
        if let Some(mut c) = child_opt.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        std::process::exit(0);
    });

    axum::serve(listener, app).await?;
    Ok(())
}
