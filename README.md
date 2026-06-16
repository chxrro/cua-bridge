# cua-bridge

A minimal MCP bridge for [cua-driver](https://github.com/trycua/cua). Spawns `cua-driver mcp`, connects to its stdio, and exposes an HTTP server with streamableHttp SSE support.

I wrote this to replace [supergateway](https://github.com/supercorp-ai/supergateway) — the reference Node.js bridge that pulls in ~200MB of dependencies and burns 40-50% CPU at idle. This is ~190 lines of Rust, compiles to a ~2MB binary, and idles at 0%.

## Build

```bash
cargo build --release
```

## Run

```bash
./target/release/cua-bridge \
  --cua-driver /Applications/CuaDriver.app/Contents/MacOS/cua-driver \
  --port 8080
```

If the bridge is on a different machine than your agent and you use Tailscale:

```bash
./target/release/cua-bridge \
  --cua-driver /path/to/cua-driver \
  --port 8080 \
  --tailscale-ip 100.64.0.1
```

Omitting `--tailscale-ip` auto-detects it. The bridge always binds localhost too.

| Flag | Default | |
|------|---------|---|
| `--cua-driver` | `/Applications/CuaDriver.app/Contents/MacOS/cua-driver` | Path to cua-driver |
| `--port` | `8080` | HTTP listen port |
| `--tailscale-ip` | auto | Tailscale IPv4 to additionally bind |

## How it works

```
Client → POST /mcp (JSON-RPC)          → forwarded to cua-driver stdio
Client → GET  /mcp (SSE streamableHttp) → broadcasts every response as events
       → GET  /health
```

Straight passthrough. No handshake modification, no caching, no buffering. Just reads a line from cua-driver's stdout and writes it to the SSE stream.

The SSE support on `GET /mcp` is why this exists. Hermes Agent's native MCP client uses streamableHttp transport and needs `GET /mcp` to return `text/event-stream`. supergateway's SSE support is unreliable. This one isn't.

## Using with Hermes Agent

```yaml
# ~/.hermes/config.yaml
mcp_servers:
  cua-remote:
    url: http://100.94.73.114:8080/mcp
```

Then `/reload-mcp`. Tools show up prefixed with `mcp_cua_remote_`.

## Why --no-overlay

cua-bridge passes `--no-overlay` to cua-driver. Without it, cua-driver v0.5.x runs a 60fps overlay render loop even in MCP stdio mode, burning 40-50% CPU ([trycua/cua#1808](https://github.com/trycua/cua/issues/1808)). The visual cursor overlay isn't needed in MCP mode — it's for the `serve`/daemon path. Once [PR #1865](https://github.com/trycua/cua/pull/1865) lands this won't be necessary.

## Linux

cua-bridge is cross-platform (Rust + axum + tokio). cua-driver has pre-release Linux binaries as of v0.5.5. Bind to localhost, use Tailscale for remote access.

## License

MIT
