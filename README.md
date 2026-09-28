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
# The public URL is the dev server, because the browser sends that origin.
cd server
AIPROXY_PUBLIC_URL=http://localhost:3000 AIPROXY_DATA_DIR=../data cargo run

# Terminal 2: dashboard with hot reload on http://localhost:3000 (API calls go to the server)
cd web
pnpm install
pnpm dev
```

To serve the dashboard from the server binary, run `pnpm build` in `web/` first, then start the server with `AIPROXY_PUBLIC_URL=http://localhost:8080`.

After a change to the admin API, run `pnpm gen:api` in `web/` to update the TypeScript types.

## First admin

On the first start there is no admin. Create a one-time setup token on the server, then open `/setup` in the browser:

```sh
docker exec aiproxy aiproxy setup-token   # or: cargo run -- setup-token
```

The token is valid for one hour. The command prints it to its own output, not to the server log.

## Configuration

| Variable | Meaning |
|---|---|
| `AIPROXY_PUBLIC_URL` | Public origin, for example `https://ai.example.com`. Required. Plain `http` is allowed only for `localhost`. |
| `AIPROXY_BIND` | Listen address. Default `0.0.0.0:8080`. |
| `AIPROXY_DATA_DIR` | Directory for the database. Default `/data`. |
| `AIPROXY_TRUSTED_PROXIES` | Comma-separated IPs or CIDRs of reverse proxies (for example Traefik) that may set `X-Forwarded-For`. Default: none. |
| `AIPROXY_LOG` | Log filter, for example `info` or `aiproxy=debug`. Default `info`. |

`aiproxy healthcheck` checks `/healthz` on the local port. The container health check uses it.
