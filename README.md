# cua-bridge

Native Rust MCP bridge for [cua-driver](https://github.com/trycua/cua). Replaces supergateway.

**~200 lines, single binary, zero runtime dependencies.**

## Why

supergateway (the reference Node.js bridge) is ~200MB of `node_modules`, burns 40-50% CPU at idle due to a tight poll loop, and doesn't properly support streamableHttp SSE — the transport Hermes Agent's native MCP client requires.

cua-bridge is a drop-in replacement:

| | supergateway | cua-bridge |
|---|---|---|
| Runtime | Node.js + 200MB deps | Native binary (~2MB) |
| Idle CPU | 40-50% (spin loop) | ~0% |
| SSE (streamableHttp) | Partial/unreliable | Full support |
| Dependencies | ~500 npm packages | 9 tiny Rust crates |
| Build | `npm install` (minutes) | `cargo build --release` (seconds) |

## Architecture

```text
Client → POST /mcp (JSON-RPC)          → cua-bridge → cua-driver stdio
Client → GET  /mcp (SSE streamableHttp) → cua-bridge (broadcasts all responses)
       → GET  /health
```

cua-bridge is a transparent proxy: it spawns `cua-driver mcp`, connects to its stdin/stdout, and exposes an HTTP server. Every tool call is forwarded verbatim — no MCP handshake manipulation. GET `/mcp` returns an SSE stream that broadcasts every response for streamableHttp clients.

## Usage

### Build

```bash
cargo build --release
```

### Run

```bash
./target/release/cua-bridge \
  --cua-driver /Applications/CuaDriver.app/Contents/MacOS/cua-driver \
  --port 8080
```

If you're on Tailscale and want the bridge reachable from other machines on your tailnet:

```bash
./target/release/cua-bridge \
  --cua-driver /Applications/CuaDriver.app/Contents/MacOS/cua-driver \
  --port 8080 \
  --tailscale-ip 100.64.0.1
```

Omitting `--tailscale-ip` auto-detects it via `tailscale ip -4`.

### Optional: activity indicator (glow socket)

If you bundle cua-bridge inside a visual app, pass `--glow-socket` and it will write `"on"` / `"off"` to a Unix socket before/after each tool call. The app listens on the socket and renders an activity indicator:

```bash
./target/release/cua-bridge \
  --cua-driver /path/to/cua-driver \
  --glow-socket ~/.hermes/halo.sock
```

Without `--glow-socket`, no signalling occurs — the bridge is a pure MCP proxy.

### Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--cua-driver <path>` | `/Applications/CuaDriver.app/Contents/MacOS/cua-driver` | Path to cua-driver binary |
| `--port <n>` | `8080` | HTTP listen port |
| `--tailscale-ip <ip>` | auto-detect | Tailscale IP to bind (falls back to localhost only) |
| `--glow-socket <path>` | none | Unix socket path for activity glow signalling |

## Integrations

### Hermes Agent

Add to `~/.hermes/config.yaml`:

```yaml
mcp_servers:
  cua-remote:
    url: http://100.94.73.114:8080/mcp
```

Then `/reload-mcp` to register the tools.

### HermesBar (macOS)

HermesBar bundles cua-bridge + cua-driver into a single `.app`. The glow socket is wired automatically — pass `--glow-socket ~/.hermes/halo.sock` and the menu bar app renders a screen-edge glow whenever a tool call is in flight.

## Known issues

### cua-driver idle CPU (40-50%)

cua-driver v0.5.x has a 60fps overlay render loop that runs even in MCP stdio mode ([trycua/cua#1808](https://github.com/trycua/cua/issues/1808)). cua-bridge launches cua-driver with `--no-overlay` to work around this. The visual cursor overlay isn't needed in MCP mode (it's for `serve`/daemon mode). Once [PR #1865](https://github.com/trycua/cua/pull/1865) lands upstream, this flag can be dropped.

### Linux support

cua-bridge itself is cross-platform (Rust + axum + tokio). cua-driver has a pre-release Linux binary as of v0.5.5. Bind to `127.0.0.1` and use Tailscale for remote access.

## License

MIT — see [LICENSE](LICENSE).
