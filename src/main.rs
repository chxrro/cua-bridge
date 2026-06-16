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
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

struct Bridge {
    child: Mutex<Child>,
    stdin: Mutex<Box<dyn Write + Send>>,
    stdout: Mutex<BufReader<Box<dyn Read + Send>>>,
    tx: broadcast::Sender<String>,
}

fn read_mcp_msg(reader: &mut BufReader<impl Read>) -> anyhow::Result<String> {
    let mut line = String::new();
    loop {
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

async fn handle_get_mcp(
    State(bridge): State<Arc<Bridge>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = bridge.tx.subscribe();
    let stream = async_stream::stream! {
        // MCP streamableHttp: first event MUST be the endpoint event
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
    // Serialize cua-driver access
    let response = {
        let mut stdin = bridge.stdin.lock();
        let mut stdout = bridge.stdout.lock();

        if let Err(e) = send_mcp_msg(&mut *stdin, body.trim()) {
            let err = serde_json::json!({
                "jsonrpc": "2.0",
                "error": {"code": -32603, "message": format!("bridge write: {}", e)},
                "id": null
            });
            bridge.tx.send(err.to_string()).ok();
            return StatusCode::ACCEPTED.into_response();
        }

        match read_mcp_msg(&mut *stdout) {
            Ok(resp) => {
                bridge.tx.send(resp.clone()).ok();
                resp
            }
            Err(e) => {
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32603, "message": format!("bridge read: {}", e)},
                    "id": null
                });
                bridge.tx.send(err.to_string()).ok();
                return StatusCode::ACCEPTED.into_response();
            }
        }
    };

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        response,
    ).into_response()
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

async fn handle_health() -> &'static str {
    "OK"
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
    let mut cua_driver = "/Applications/CuaDriver.app/Contents/MacOS/cua-driver"
        .to_string();
    let mut port: u16 = 8080;
    let mut extra_bind: Option<IpAddr> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cua-driver" => { i += 1; cua_driver = args[i].clone(); }
            "--port" => { i += 1; port = args[i].parse()?; }
            "--tailscale-ip" => { i += 1; extra_bind = Some(args[i].parse()?); }
            other => {
                eprintln!("Unknown flag: {}", other);
                eprintln!("Usage: cua-bridge [--cua-driver <path>] [--port <n>] [--tailscale-ip <ip>]");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    tracing::info!("Starting cua-driver: {}", cua_driver);
    let mut child = Command::new(&cua_driver)
        .arg("mcp")
        // --no-overlay prevents the 60fps idle overlay render loop from
        // burning 40-50% CPU in MCP stdio mode (trycua/cua#1808).
        // Not needed in serve/daemon mode where the visual cursor is wanted.
        .arg("--no-overlay")
        .arg("--no-daemon-relaunch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    tracing::info!("cua-driver pid {}", child.id());

    let (tx, _) = broadcast::channel::<String>(64);
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let bridge = Arc::new(Bridge {
        child: Mutex::new(child),
        stdin: Mutex::new(Box::new(child_stdin)),
        stdout: Mutex::new(BufReader::new(
            Box::new(child_stdout) as Box<dyn Read + Send>
        )),
        tx,
    });

    let app = Router::new()
        .route("/mcp", get(handle_get_mcp).post(handle_post_mcp).options(handle_options))
        .route("/health", get(handle_health))
        .with_state(bridge.clone());

    let tailscale_ip = extra_bind.or_else(detect_tailscale_ip);
    let mut addrs: Vec<SocketAddr> = Vec::new();
    if let Some(ts) = tailscale_ip {
        addrs.push(SocketAddr::new(ts, port));
    }
    addrs.push(SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port));

    let mut listeners: Vec<TcpListener> = Vec::new();
    for addr in &addrs {
        match TcpListener::bind(*addr).await {
            Ok(l) => {
                tracing::info!("listening on http://{}", addr);
                listeners.push(l);
            }
            Err(e) => {
                tracing::warn!("could not bind {}: {}", addr, e);
            }
        }
    }
    if listeners.is_empty() {
        anyhow::bail!("no bind address");
    }
    tracing::info!("cua-bridge ready");

    let bs = bridge.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let mut c = bs.child.lock();
        let _ = c.kill();
        let _ = c.wait();
        std::process::exit(0);
    });

    // Serve on all bound addresses concurrently
    let mut handles = Vec::with_capacity(listeners.len());
    for listener in listeners {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            axum::serve(listener, app).await
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
    Ok(())
}
