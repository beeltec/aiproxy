# Changelog

All notable changes to this project are in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- aiproxy now sends the current Codex CLI version to the ChatGPT backend, in the `client_version` of the model list request and in the `User-Agent` of all backend requests. The backend hides models that need a newer client, so new models did not show. aiproxy reads the latest version from the npm registry at start and then again when it is older than a day. When npm is not available, it uses the last known version, else a built-in version (`0.159.2`). ([#26](https://github.com/beeltec/aiproxy/pull/26))

## [0.1.1] - 2026-09-30

### Fixed

- `/v1/responses` accepts `item_reference` items and id-only items in `input`. Clients such as `@ai-sdk/openai` send them on the second turn of a tool conversation, and aiproxy rejected them with HTTP 400. aiproxy now keeps the output items of each answer in memory for 24 hours, per API key. An unknown or expired id gives HTTP 400, and more than 16 MiB of resolved items gives HTTP 413. ([#24](https://github.com/beeltec/aiproxy/pull/24))

## [0.1.0] - 2026-09-29

### Added

- Endpoints `/v1/responses`, `/v1/chat/completions`, `/v1/messages`, `/v1/messages/count_tokens` and `/v1/models`, with and without streaming. Any client format works with any provider.
- Endpoints for embeddings, speech, transcription, translation, and image generation and edits.
- Upstreams: ChatGPT subscriptions (device code or browser sign-in, token refresh, failover between accounts) and API keys of OpenAI, Anthropic and OpenRouter.
- Mapping of reasoning effort, fast mode, reasoning summaries, function tools, web search and the image-generation tool between the formats.
- Models `chatgpt/<model>` and `<connection>/<model>`, enabled per model, and aliases with defaults.
- Gateway API keys with expiry, request and token limits, and a list of allowed models.
- Usage and cost of each request, with prices from LiteLLM, models.dev and OpenRouter, overrides and recompute.
- Dashboard: overview, usage, pricing, subscriptions, connections, aliases, API keys, admins and settings.
- Admin login with passwords, authenticator apps (TOTP), passkeys and recovery codes.
- Docker image for `linux/amd64` and `linux/arm64`, signed with Sigstore, and a compose setup with Traefik.

[Unreleased]: https://github.com/beeltec/aiproxy/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/beeltec/aiproxy/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/beeltec/aiproxy/releases/tag/v0.1.0
