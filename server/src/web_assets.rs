use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode, Uri, header};
use base64::Engine;
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};

#[derive(RustEmbed)]
#[folder = "../web/dist/client"]
#[allow_missing = true]
struct Assets;

const SHELL: &str = "_shell.html";

/// Paths that belong to the server. The SPA fallback must not answer them.
const SERVER_PREFIXES: [&str; 3] = ["/v1/", "/admin/api/", "/healthz"];

#[derive(Clone)]
pub struct WebAssets {
    shell: Option<Arc<str>>,
    csp: HeaderValue,
}

impl WebAssets {
    pub fn new() -> Self {
        // The TanStack shell has raw NUL characters in JS strings. The HTML parser changes them to
        // U+FFFD, which breaks hydration. The JS escape keeps the same string value.
        let shell: Option<Arc<str>> =
            Assets::get(SHELL).map(|file| String::from_utf8_lossy(&file.data).replace('\0', "\\u0000").into());
        let script_hashes = shell.as_deref().map(inline_script_hashes).unwrap_or_default();
        let script_src = std::iter::once("'self'".to_owned())
            .chain(script_hashes.iter().map(|h| format!("'sha256-{h}'")))
            .collect::<Vec<_>>()
            .join(" ");
        let csp = format!(
            "default-src 'self'; script-src {script_src}; style-src 'self'; img-src 'self' data:; \
             font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
             form-action 'self'; frame-ancestors 'none'"
        );
        Self {
            shell,
            csp: HeaderValue::from_str(&csp).expect("CSP is a valid header value"),
        }
    }

    pub fn serve(&self, uri: &Uri) -> Response<Body> {
        let path = uri.path();
        if SERVER_PREFIXES
            .iter()
            .any(|p| path.starts_with(p) || path == p.trim_end_matches('/'))
        {
            return empty(StatusCode::NOT_FOUND);
        }

        let file_path = path.trim_start_matches('/');
        if let Some(file) = Assets::get(file_path).filter(|_| !file_path.is_empty() && file_path != SHELL) {
            let cache = if file_path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            return Response::builder()
                .header(header::CONTENT_TYPE, file.metadata.mimetype())
                .header(header::CACHE_CONTROL, cache)
                .body(Body::from(file.data.into_owned()))
                .expect("static response is valid");
        }

        // Missing files below /assets/ are real 404s, not SPA routes.
        if file_path.starts_with("assets/") {
            return empty(StatusCode::NOT_FOUND);
        }

        match &self.shell {
            Some(shell) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::CONTENT_SECURITY_POLICY, self.csp.clone())
                .body(Body::from(shell.to_string()))
                .expect("shell response is valid"),
            None => empty(StatusCode::NOT_FOUND),
        }
    }
}

fn empty(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("empty response is valid")
}

/// Returns the base64 SHA-256 of each inline script, for the CSP `script-src` list.
fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        rest = &rest[start..];
        let Some(open_end) = rest.find('>') else { break };
        let open_tag = &rest[..open_end];
        let body = &rest[open_end + 1..];
        let Some(close) = body.find("</script>") else { break };
        if !open_tag.contains(" src=") {
            let digest = Sha256::digest(&body.as_bytes()[..close]);
            hashes.push(base64::engine::general_purpose::STANDARD.encode(digest));
        }
        rest = &body[close..];
    }
    hashes
}
