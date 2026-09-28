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
export AIPROXY_MASTER_KEY=$(openssl rand -base64 32)   # keep it: secrets in the database need it
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

## ChatGPT subscriptions

Link accounts on the Subscriptions page. The default is the device code: the dashboard shows a code that you enter at https://auth.openai.com/codex/device (device login must be allowed in the ChatGPT security settings). The other way is a browser sign-in: at the end the browser opens `http://127.0.0.1:1455/auth/callback?...` and shows an error; paste that address into the dashboard.

The gateway renews the tokens on a cron plan (Settings page, default `0 0 * * *` in the instance time zone) and when a request needs it. Each account can use its own plan or none. OpenAI does not officially support the use of a subscription outside its own apps. Use it at your own risk.

## Second factors

Each admin can add an authenticator app (TOTP) and passkeys on the Security page. With a second factor, a password alone does not log in. A passkey can also log in without a password. The first second factor gives 10 one-time recovery codes. An admin who lost all factors can get a reset from another admin (Admins page).

Passkeys are bound to the host name of `AIPROXY_PUBLIC_URL`. If the host name changes, passkeys must be added again.

## Configuration

| Variable | Meaning |
|---|---|
| `AIPROXY_PUBLIC_URL` | Public origin, for example `https://ai.example.com`. Required. Plain `http` is allowed only for `localhost`. |
| `AIPROXY_MASTER_KEY` | 32 random bytes, base64 (`openssl rand -base64 32`). Encrypts secrets in the database, for example TOTP keys. Required. If you lose it, all admins must set up TOTP again. |
| `AIPROXY_MASTER_KEY_FILE` | Path to a file with the master key (for Docker secrets). Use this or `AIPROXY_MASTER_KEY`. |
| `AIPROXY_BIND` | Listen address. Default `0.0.0.0:8080`. |
| `AIPROXY_DATA_DIR` | Directory for the database. Default `/data`. |
| `AIPROXY_TRUSTED_PROXIES` | Comma-separated IPs or CIDRs of reverse proxies (for example Traefik) that may set `X-Forwarded-For`. Default: none. |
| `AIPROXY_LOG` | Log filter, for example `info` or `aiproxy=debug`. Default `info`. |

`aiproxy healthcheck` checks `/healthz` on the local port. The container health check uses it.
