# aiproxy

A self-hosted gateway that exposes OpenAI-compatible and Anthropic-compatible (Claude Code) endpoints. It sends requests to a linked ChatGPT subscription or to keyed providers (OpenAI, Anthropic, OpenRouter), and records token usage and API-equivalent costs.

Status: in development.

## Repository layout

- `server/` – Rust binary (`aiproxy`). Serves the gateway, the admin API and the embedded dashboard.
- `web/` – Admin dashboard (TanStack Start in SPA mode). The build output in `web/dist/client` is embedded into the server binary.

## Development

Requirements: Rust (the version in `rust-toolchain.toml` is installed automatically by rustup), Node 24, pnpm 10.

```sh
# Terminal 1: server on http://localhost:8080
cd server
AIPROXY_PUBLIC_URL=http://localhost:8080 AIPROXY_DATA_DIR=../data cargo run

# Terminal 2: dashboard with hot reload on http://localhost:3000 (API calls go to the server)
cd web
pnpm install
pnpm dev
```

To serve the dashboard from the server binary, run `pnpm build` in `web/` first, then start the server.

## Configuration

| Variable | Meaning |
|---|---|
| `AIPROXY_PUBLIC_URL` | Public origin, for example `https://ai.example.com`. Required. Plain `http` is allowed only for `localhost`. |
| `AIPROXY_BIND` | Listen address. Default `0.0.0.0:8080`. |
| `AIPROXY_DATA_DIR` | Directory for the database. Default `/data`. |
| `AIPROXY_LOG` | Log filter, for example `info` or `aiproxy=debug`. Default `info`. |

`aiproxy healthcheck` checks `/healthz` on the local port. The container health check uses it.
