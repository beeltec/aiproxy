# aiproxy

A self-hosted gateway with OpenAI-compatible and Anthropic-compatible (Claude Code) endpoints. It sends requests to a linked ChatGPT subscription or to providers with an API key (OpenAI, Anthropic, OpenRouter). It records the token usage of every request and its cost at API prices.

## Deployment

The image is `ghcr.io/beeltec/aiproxy`. The tag `main` follows the main branch; each commit also has a `sha-<commit>` tag, and releases have version tags (`1.2.3`, `1.2`). It runs as a non-root user, listens on port 8080 and keeps its database in `/data`.

`deploy/compose.yaml` runs aiproxy behind Traefik with TLS certificates from Let's Encrypt. On the server:

```sh
mkdir aiproxy && cd aiproxy          # copy deploy/compose.yaml into this folder
openssl rand -base64 32 > master.key
sudo chown 65532:65532 master.key    # the user in the image
sudo chmod 400 master.key
cat > .env <<'EOF'
AIPROXY_HOST=ai.example.com
ACME_EMAIL=you@example.com
EOF
docker compose up -d
```

The DNS name must point to the server, and ports 80 and 443 must be open. The image is private: log in first with `docker login ghcr.io` and a GitHub token with the `read:packages` scope.

Traefik and aiproxy share a network where Traefik has a fixed address. `AIPROXY_TRUSTED_PROXIES` names that address, so only Traefik can set the client address (`X-Forwarded-For`). If Traefik already runs on the server, remove the `traefik` service and attach your Traefik to the `aiproxy_proxy` network with the address `172.30.0.2`.

Keep `master.key` and the `data` volume. The key encrypts the secrets in the database (provider keys, ChatGPT tokens, authenticator keys). Without it, those secrets cannot be read and must be added again.

Update with `docker compose pull && docker compose up -d`. At a stop, running requests get 30 seconds to end and record their usage.

### First admin

On the first start there is no admin. Create a one-time setup token, then open `https://ai.example.com/setup`:

```sh
docker compose exec aiproxy aiproxy setup-token
```

The token is valid for one hour. The command prints it to its own output, not to the server log.

## Models and providers

- **ChatGPT subscriptions** (Subscriptions page): link an account with a device code, which you enter at https://auth.openai.com/codex/device (device login must be allowed in the ChatGPT security settings), or with a browser sign-in: at its end the browser opens `http://127.0.0.1:1455/auth/callback?...` and shows an error; paste that address into the dashboard. The gateway renews the tokens on a plan (Settings page) and when a request needs it. With more accounts, failover can move requests to the next account when one reaches its usage limit. OpenAI does not officially support the use of a subscription outside its own apps. Use it at your own risk.
- **Connections** (Connections page): add an API key of OpenAI, Anthropic or OpenRouter. The gateway loads the model list; enable the models that clients may use on the Models tab.
- **Aliases** (Aliases page): a name that points to a model and can set defaults (reasoning effort, fast mode, reasoning summary). The page has a ready Claude Code setup.

Model names are `chatgpt/<model>` and `<connection>/<model>`, for example `chatgpt/gpt-5.5` or `anthropic/claude-sonnet-5`. A bare name works when only one enabled model has it.

## Using the gateway

Create an API key on the API keys page. A key can have an expiry, request and token limits, and a list of the models it may use.

Claude Code:

```sh
export ANTHROPIC_BASE_URL=https://ai.example.com
export ANTHROPIC_AUTH_TOKEN=sk-aip-...
export ANTHROPIC_MODEL=chatgpt/gpt-5.5
export ANTHROPIC_DEFAULT_HAIKU_MODEL=chatgpt/gpt-6-luna
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=272000
```

OpenAI SDKs and tools:

```sh
export OPENAI_BASE_URL=https://ai.example.com/v1
export OPENAI_API_KEY=sk-aip-...
```

Endpoints:

- `/v1/responses`, `/v1/chat/completions`, `/v1/messages`, `/v1/messages/count_tokens` and `/v1/models`, with and without streaming. Any client format works with any provider; the gateway translates between them. Reasoning effort (`reasoning.effort`, `reasoning_effort`, `output_config.effort` or the Anthropic thinking budget) and fast mode (`service_tier: "fast"` or `"priority"`, Anthropic `speed: "fast"`) are mapped between the formats.
- `/v1/embeddings` (OpenAI and OpenRouter connections).
- `/v1/audio/speech`, `/v1/audio/transcriptions` and `/v1/audio/translations` (OpenAI connections).
- `/v1/images/generations` and `/v1/images/edits` (OpenAI connections, and ChatGPT models through the image-generation tool).

Web search and the image-generation tool work as hosted tools. Other hosted tools, stored server state (`previous_response_id`, conversations, file ids) and stored files are refused: send the full conversation and the content inline.

## Usage and prices

The Overview and Usage pages show requests, tokens and their cost at API prices ("API value") per model, API key or upstream, for any time range in your time zone.

The prices come from the public lists of LiteLLM, models.dev and OpenRouter. The gateway loads them daily on a plan (Settings page, default 04:00) and on the Pricing page on demand. Each request gets its cost when it is recorded. An override on the Pricing page sets the prices of one model; "Recompute" gives stored requests the current prices. A cost is marked incomplete when the usage is an estimate or a price is missing.

## Second factors

Each admin can add an authenticator app (TOTP) and passkeys on the Security page. With a second factor, a password alone does not log in. A passkey can also log in without a password. The first second factor gives 10 one-time recovery codes. An admin who lost all factors can get a reset from another admin (Admins page).

Passkeys are bound to the host name of `AIPROXY_PUBLIC_URL`. If the host name changes, passkeys must be added again.

## Configuration

| Variable | Meaning |
|---|---|
| `AIPROXY_PUBLIC_URL` | Public origin, for example `https://ai.example.com`. Required. Plain `http` is allowed only for `localhost`. |
| `AIPROXY_MASTER_KEY` | 32 random bytes, base64 (`openssl rand -base64 32`). Encrypts the secrets in the database. Required. |
| `AIPROXY_MASTER_KEY_FILE` | Path to a file with the master key (for Docker secrets). Use this or `AIPROXY_MASTER_KEY`. |
| `AIPROXY_BIND` | Listen address. Default `0.0.0.0:8080`. |
| `AIPROXY_DATA_DIR` | Directory for the database. Default `/data`. |
| `AIPROXY_TRUSTED_PROXIES` | Comma-separated IPs or CIDRs of reverse proxies (for example Traefik) that may set `X-Forwarded-For`. Default: none. |
| `AIPROXY_ALLOW_PRIVATE_UPSTREAMS` | `true` allows connection base URLs on private or local addresses (for example a local model server). Default `false`. |
| `AIPROXY_LOG` | Log filter, for example `info` or `aiproxy=debug`. Default `info`. |

Other settings (time zone, refresh and price sync plans, failover) are on the Settings page.

`aiproxy healthcheck` checks `/healthz` on the local port. The container health check uses it.

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

To serve the dashboard from the server binary, run `pnpm build` in `web/` first, then start the server with `AIPROXY_PUBLIC_URL=http://localhost:8080`. `cargo run -- setup-token` creates the setup token.

After a change to the admin API, run `pnpm gen:api` in `web/` to update the TypeScript types; CI checks that they match.

- `server/` – the Rust binary (`aiproxy`): the gateway, the admin API and the embedded dashboard.
- `web/` – the admin dashboard (TanStack Start in SPA mode). Its build output in `web/dist/client` is embedded into the server binary.
- `deploy/` – the compose file for Traefik.
