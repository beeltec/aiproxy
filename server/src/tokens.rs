//! Local token estimate (o200k), for limits and for usage that the upstream did not report.

use std::sync::LazyLock;

use serde_json::Value;
use tiktoken_rs::CoreBPE;
use tokio::sync::Semaphore;

static O200K: LazyLock<CoreBPE> = LazyLock::new(|| tiktoken_rs::o200k_base().expect("o200k tables load"));

/// Pieces longer than this are split at whitespace before BPE; runs without whitespace that are
/// longer count as bytes / 4. This bounds the work per request.
const MAX_PIECE: usize = 64 * 1024;
/// Above this much text the estimate is bytes / 4 without BPE.
const MAX_TEXT: usize = 4 * 1024 * 1024;
/// Rough cost of one image in tokens.
const IMAGE_TOKENS: usize = 800;

fn count_text(text: &str) -> usize {
    if text.len() > MAX_TEXT {
        return text.len() / 4;
    }
    let mut total = 0;
    let mut piece_start = 0;
    let mut last_space = None;
    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() {
            last_space = Some(index);
        }
        if index - piece_start >= MAX_PIECE {
            match last_space.filter(|s| *s > piece_start) {
                Some(split) => {
                    total += O200K.encode_ordinary(&text[piece_start..split]).len();
                    piece_start = split;
                }
                None => {
                    total += (index - piece_start) / 4;
                    piece_start = index;
                }
            }
        }
    }
    total + O200K.encode_ordinary(&text[piece_start..]).len()
}

/// At most this many estimates run at the same time, on the blocking thread pool.
static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

/// Estimates the input tokens of a request body off the async threads.
pub async fn estimate(body: &Value) -> usize {
    let _slot = SLOTS.acquire().await.expect("the estimate semaphore is never closed");
    let body = body.clone();
    tokio::task::spawn_blocking(move || count_request(&body))
        .await
        .unwrap_or(0)
}

/// Estimates the input tokens of a request body: all text values, plus a fixed amount per image.
fn count_request(body: &Value) -> usize {
    let mut text = String::new();
    let mut images = 0;
    collect(body, &mut text, &mut images);
    count_text(&text) + images * IMAGE_TOKENS
}

fn collect(value: &Value, text: &mut String, images: &mut usize) {
    match value {
        Value::String(s) if s.starts_with("data:") => *images += 1,
        Value::String(s) => {
            text.push_str(s);
            text.push(' ');
        }
        Value::Array(items) => items.iter().for_each(|item| collect(item, text, images)),
        Value::Object(map) => {
            if matches!(
                map.get("type").and_then(Value::as_str),
                Some("input_image" | "image_url" | "image")
            ) {
                *images += 1;
                return;
            }
            map.values().for_each(|item| collect(item, text, images));
        }
        _ => {}
    }
}
