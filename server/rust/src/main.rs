//! Example backend for the direct-integration path.
//!
//! Two routes:
//! ```text
//! POST /api/seel/quote  - browser quote proxy: attaches the server-side API
//!                         key and forwards to Seel's Quote API (the widget
//!                         never sees the key)
//! POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
//!                         events: verifies HMAC, ACKs 200 fast, then hands
//!                         off for internal fan-out
//! ```
//!
//! Run:
//! ```text
//! SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... cargo run
//! ```
//!
//! To drive the widget demo against a live sandbox, two steps - this server
//! doesn't serve the demo page:
//! 1. run this server
//! 2. in widget/demo.html, replace the mock quoteFetcher with
//!    configure({ quoteEndpoint: "http://localhost:8787/api/seel/quote" })

use std::sync::Arc;
use std::thread;

use seel_direct_sdk::{verify_webhook_signature, SeelClient, SeelError, SANDBOX_BASE_URL};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

struct Config {
    client: SeelClient,
    webhook_secret: String,
    // Program values from Seel onboarding. When set, the proxy stamps them
    // into every quote request, so storefront code stays identical across
    // programs.
    merchant_id: String,
    quote_type: String,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Internal fan-out. Map merchant_id/order_id to your own retailer code
/// here and route to your systems. Dedupe on id + type first, since
/// delivery is at-least-once. In production, queue this work off the
/// request thread instead of processing inline.
fn handle_webhook_event(event: &Value) {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("unknown");
    let event_id = event.get("id").and_then(Value::as_str).unwrap_or("unknown");
    println!("[webhook] {event_type} id={event_id}");
}

/// Build a header from static name/value bytes (always well-formed).
fn header(name: &[u8], value: &[u8]) -> Header {
    match Header::from_bytes(name, value) {
        Ok(h) => h,
        Err(()) => unreachable!("static header bytes are always well-formed"),
    }
}

fn respond_json(request: Request, status: u16, body: &Value) {
    let response = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(header(b"Content-Type", b"application/json"))
        // Demo only; lock down in prod.
        .with_header(header(b"Access-Control-Allow-Origin", b"*"));
    if let Err(err) = request.respond(response) {
        eprintln!("failed to send response: {err}");
    }
}

/// CORS preflight for the demo page.
fn respond_preflight(request: Request) {
    let response = Response::empty(204)
        // Demo only; lock down in prod.
        .with_header(header(b"Access-Control-Allow-Origin", b"*"))
        .with_header(header(b"Access-Control-Allow-Headers", b"Content-Type"))
        .with_header(header(b"Access-Control-Allow-Methods", b"POST, OPTIONS"));
    if let Err(err) = request.respond(response) {
        eprintln!("failed to send response: {err}");
    }
}

fn read_body(request: &mut Request) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    match request.as_reader().read_to_end(&mut body) {
        Ok(_) => Some(body),
        Err(_) => None,
    }
}

fn handle_quote(mut request: Request, config: &Config) {
    let raw = match read_body(&mut request) {
        Some(b) => b,
        None => {
            respond_json(request, 400, &json!({"error": "request body must be JSON"}));
            return;
        }
    };
    let mut params: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => {
            respond_json(request, 400, &json!({"error": "request body must be JSON"}));
            return;
        }
    };
    if let Some(obj) = params.as_object_mut() {
        if !config.merchant_id.is_empty() {
            obj.insert("merchant_id".to_string(), Value::String(config.merchant_id.clone()));
        }
        if !config.quote_type.is_empty() {
            obj.insert("type".to_string(), Value::String(config.quote_type.clone()));
        }
    }
    match config.client.create_quote(&params) {
        Ok(resp) => respond_json(request, 200, &resp),
        Err(SeelError::Api(err)) => {
            // Forward Seel's status and error body - it names the
            // offending field.
            let body = if err.body.is_object() {
                err.body.clone()
            } else {
                json!({"error": err.to_string()})
            };
            respond_json(request, err.status, &body);
        }
        Err(_) => respond_json(request, 502, &json!({"error": "upstream quote request failed"})),
    }
}

fn handle_webhook(mut request: Request, config: &Config) {
    // An unreadable body can't be verified, so treat it as unsigned.
    let raw = match read_body(&mut request) {
        Some(b) => b,
        None => {
            respond_json(request, 401, &json!({"error": "invalid signature"}));
            return;
        }
    };
    let signature = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("X-Seel-Hmac-SHA256"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    if !verify_webhook_signature(&raw, &signature, &config.webhook_secret) {
        respond_json(request, 401, &json!({"error": "invalid signature"}));
        return;
    }
    // ACK and flush before doing any work: Seel retries anything not
    // answered with a 200 within 10 seconds. respond() writes and flushes
    // the full response before returning.
    respond_json(request, 200, &json!({"ok": true}));
    // Already ACKed; never let a processing failure escape this handler.
    match serde_json::from_slice::<Value>(&raw) {
        Ok(event) => handle_webhook_event(&event),
        Err(err) => eprintln!("[webhook] processing error: {err}"),
    }
}

fn handle_request(request: Request, config: &Config) {
    // Copy method and URL out first: the handlers consume the request when
    // they respond.
    let method = request.method().clone();
    let url = request.url().to_string();
    match (method, url.as_str()) {
        (Method::Options, _) => respond_preflight(request),
        (Method::Post, "/api/seel/quote") => handle_quote(request, config),
        (Method::Post, "/webhooks/seel") => handle_webhook(request, config),
        _ => respond_json(request, 404, &json!({"error": "not found"})),
    }
}

fn main() {
    let port: u16 = match env_or("PORT", "8787").parse() {
        Ok(p) => p,
        Err(_) => 8787,
    };
    let api_key = env_or("SEEL_API_KEY", "");
    let base_url = env_or("SEEL_BASE_URL", SANDBOX_BASE_URL);

    if api_key.is_empty() {
        println!("warning: SEEL_API_KEY not set - quote proxy will fail");
    }

    let config = Arc::new(Config {
        client: SeelClient::new(&api_key, &base_url),
        webhook_secret: env_or("SEEL_WEBHOOK_SECRET", ""),
        merchant_id: env_or("SEEL_MERCHANT_ID", ""),
        quote_type: env_or("SEEL_QUOTE_TYPE", ""),
    });

    let server = match Server::http(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to bind port {port}: {err}");
            std::process::exit(1);
        }
    };
    println!("listening on http://localhost:{port}");

    // Handle each request on its own thread so webhook deliveries do not
    // queue behind each other.
    for request in server.incoming_requests() {
        let config = Arc::clone(&config);
        thread::spawn(move || handle_request(request, &config));
    }
}
