//! Example backend for the direct-integration path.
//!
//! Routes:
//! ```text
//! POST /v1/ecommerce/quotes  - browser quote proxy: attaches the server-side API
//!                         key and forwards to Seel's Quote API (the widget
//!                         never sees the key)
//!
//! POST /v1/ecommerce/orders                              - create order
//! POST /v1/ecommerce/orders/{order_id}                   - update order
//! POST /v1/ecommerce/orders/{order_id}/cancel            - cancel order
//! POST /v1/ecommerce/orders/{order_id}/fulfillments      - create fulfillment
//! POST /v1/ecommerce/orders/{order_id}/fulfillments/{id} - update fulfillment
//!
//! POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
//!                         events: verifies HMAC, ACKs 200 fast, then hands
//!                         off for internal fan-out
//! ```
//!
//! The order and fulfillment routes mirror Seel's own path shape, so a caller
//! already written against Seel's API moves over by changing the base URL and
//! nothing else.
//!
//! Two deployments use these routes differently:
//!
//! - Single retailer - the retailer's own backend holds the API key and calls
//!   Seel directly. The order and fulfillment routes are optional here; call
//!   [`SeelClient`] from your order pipeline instead if that fits better.
//! - Platform proxy - the platform holds one API key for every retailer on it,
//!   retailers point at the platform instead of at Seel, and the platform
//!   resolves which merchant each request belongs to. Retailers hold no Seel
//!   credentials at all. See [`authenticate_caller`] and
//!   [`resolve_merchant_id`], which are where a platform starts. They are
//!   not the whole job: `order_id` comes off the URL and is never checked
//!   against the caller, so nothing here stops one retailer touching
//!   another's order. That mapping belongs to the platform - see the
//!   README.
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
//!    configure({ quoteEndpoint: "http://localhost:8787/v1/ecommerce/quotes" })

use std::io::Read;
use std::sync::Arc;
use std::thread;

use seel_direct_integration_reference::{
    parse_route, verify_webhook_signature, Route, SeelClient, SeelError, SANDBOX_BASE_URL,
};
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

/// Before any of this fires, the endpoint has to be registered with Seel.
/// Seel has no self-serve way to register this URL. There is no webhook field on
/// Create or Update Merchant and no registration endpoint - ask your Seel contact
/// to configure it, and tell them which events you want. Do it once per
/// environment: a sandbox registration does not carry over to production.
///
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

/// The largest request body any route accepts. A quote or order payload is
/// a few KB; without a cap one 200 MB POST sits in this process's memory.
const MAX_BODY_BYTES: usize = 1 << 20;

/// Why a request body was not accepted, with the status it answers.
#[derive(Debug)]
struct BodyReject {
    status: u16,
    error: String,
}

impl BodyReject {
    fn too_large() -> Self {
        BodyReject {
            status: 413,
            error: format!("request body exceeds {MAX_BODY_BYTES} bytes"),
        }
    }

    fn bad_request(error: &str) -> Self {
        BodyReject { status: 400, error: error.to_string() }
    }
}

/// Read a body of at most `MAX_BODY_BYTES`. The declared Content-Length
/// is refused up front so nothing is read; a chunked or lying body is cut
/// off while reading.
fn read_capped(declared_length: Option<usize>, reader: &mut dyn Read) -> Result<Vec<u8>, BodyReject> {
    if declared_length.is_some_and(|n| n > MAX_BODY_BYTES) {
        return Err(BodyReject::too_large());
    }
    let mut body = Vec::new();
    reader
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| BodyReject::bad_request("could not read request body"))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(BodyReject::too_large());
    }
    Ok(body)
}

fn read_body(request: &mut Request) -> Result<Vec<u8>, BodyReject> {
    let result = read_capped(request.body_length(), request.as_reader());
    // A refused body has to be consumed to its end here. tiny_http 0.12
    // parses whatever is left in the socket as the next request on the
    // connection, and gives a handler no way to close it instead: close is
    // decided from the request's own headers only, and a Connection header
    // on the response is discarded. Leaving a chunked remainder in place
    // let a request hidden inside an oversized chunk run. Draining in 8 KB
    // steps also sidesteps EqualReader::drop, which otherwise allocates the
    // whole remaining Content-Length at once. The cost is that the 413
    // lands only once the body has.
    if result.is_err() {
        let _ = std::io::copy(request.as_reader(), &mut std::io::sink());
    }
    result
}

/// Decide whether the caller may use this proxy.
///
/// This demo accepts everyone, which is only safe because it holds a sandbox
/// key and listens on localhost.
///
/// A platform MUST replace this. Retailers authenticate to the platform with
/// platform credentials - they never receive a Seel API key, because one key
/// covers every retailer on the platform and would let any holder act as any
/// other. A real implementation returns the caller's identity rather than
/// a bool, and [`resolve_merchant_id`] takes it - changing both signatures
/// is part of the work.
fn authenticate_caller(_request: &Request) -> bool {
    true
}

/// Return the merchant ID this request belongs to.
///
/// Single retailer: `SEEL_MERCHANT_ID` is set once in the environment and
/// stamped onto everything, so storefront and pipeline code carry no
/// program-specific values.
///
/// Platform proxy: leave `SEEL_MERCHANT_ID` unset and look the merchant up
/// from the authenticated caller instead. Deriving it from the caller rather
/// than trusting the request body is what stops one retailer quoting or
/// ordering against another's merchant ID.
///
/// The body fallback below is a demo default so the unconfigured server
/// still runs. It is not safe on a real platform: any caller can name any
/// merchant. Replace it before anyone but you can reach this.
fn resolve_merchant_id(config: &Config, params: &Value) -> String {
    if !config.merchant_id.is_empty() {
        return config.merchant_id.clone();
    }
    params
        .get("merchant_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Map a Seel result onto the status and body the caller gets.
///
/// Each failure has to stay distinguishable. Collapsing them into one 502
/// would report a payload that never left this process as an upstream
/// outage, and would invite a retry on an order Seel has already accepted.
fn map_result(label: &str, result: Result<Value, SeelError>) -> (u16, Value) {
    // The request never left this process, so this is the caller's bug.
    // Hand back every problem at once.
    let caller_bug = |operation: String, problems: Vec<String>| {
        (
            400,
            json!({
                "error": format!("{operation}: {}", problems.join("; ")),
                "problems": problems,
            }),
        )
    };
    match result {
        Ok(resp) => (200, resp),
        Err(SeelError::Validation { operation, problems }) => caller_bug(operation, problems),
        Err(SeelError::InvalidId { operation, problem }) => caller_bug(operation, vec![problem]),
        Err(SeelError::ContractNotMinted { detail, response }) => {
            // Seel accepted the order and minted no contract. 502 would be
            // wrong twice over: the upstream call succeeded, and a retry
            // would duplicate the order.
            println!("[proxy] {label}: {detail}");
            (
                409,
                json!({
                    "error": format!("order created but no contract was minted: {detail}"),
                    "seel_response": *response,
                }),
            )
        }
        Err(SeelError::Api(err)) => {
            // Forward Seel's status and error body - it names the
            // offending field.
            let body = if err.body.is_object() {
                err.body.clone()
            } else {
                json!({"error": err.to_string()})
            };
            (err.status, body)
        }
        Err(other) if other.is_timeout() => {
            // Seel got the request and never answered, so it may have been
            // processed. A 502 here would invite the retry that double
            // creates an order.
            eprintln!("[proxy] {label} timed out: {other}");
            (
                504,
                json!({
                    "error": "timed out waiting for Seel; the request may have been processed, check before retrying"
                }),
            )
        }
        Err(other) => {
            // Log before answering: a bare 502 leaves the operator unable
            // to tell a reset from a bug in this handler.
            eprintln!("[proxy] {label} failed: {other}");
            (502, json!({ "error": format!("upstream {label} request failed") }))
        }
    }
}

fn respond_result(request: Request, label: &str, result: Result<Value, SeelError>) {
    let (status, body) = map_result(label, result);
    respond_json(request, status, &body);
}

/// Read and parse a JSON object body, or return the rejection to send back.
fn read_json_object(request: &mut Request) -> Result<Value, BodyReject> {
    // A body that never fully arrived is not the same as a malformed one,
    // and saying so would send the caller looking at the wrong thing.
    let raw = read_body(request)?;
    let parsed: Value = serde_json::from_slice(&raw)
        .map_err(|_| BodyReject::bad_request("request body must be JSON"))?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(BodyReject::bad_request("request body must be a JSON object"))
    }
}

fn respond_reject(request: Request, reject: BodyReject) {
    respond_json(request, reject.status, &json!({ "error": reject.error }));
}

fn handle_quote(mut request: Request, config: &Config) {
    let mut params = match read_json_object(&mut request) {
        Ok(v) => v,
        Err(reject) => {
            respond_reject(request, reject);
            return;
        }
    };
    let merchant_id = resolve_merchant_id(config, &params);
    if let Some(obj) = params.as_object_mut() {
        if !merchant_id.is_empty() {
            obj.insert("merchant_id".to_string(), Value::String(merchant_id));
        }
        if !config.quote_type.is_empty() {
            obj.insert("type".to_string(), Value::String(config.quote_type.clone()));
        }
    }
    let result = config.client.create_quote(&params);
    respond_result(request, "quote", result);
}

/// Sync every order, opted in or not. On opt-in the body carries
/// seel_services with the quote_id and price, which mints the contract and
/// fires contract.created.
fn handle_create_order(mut request: Request, config: &Config) {
    let mut params = match read_json_object(&mut request) {
        Ok(v) => v,
        Err(reject) => {
            respond_reject(request, reject);
            return;
        }
    };
    let merchant_id = resolve_merchant_id(config, &params);
    if let Some(obj) = params.as_object_mut() {
        if !merchant_id.is_empty() {
            obj.insert("merchant_id".to_string(), Value::String(merchant_id));
        }
    }
    let result = config.client.create_order(&params);
    respond_result(request, "order", result);
}

fn handle_update_order(mut request: Request, config: &Config, order_id: &str) {
    let params = match read_json_object(&mut request) {
        Ok(v) => v,
        Err(reject) => {
            respond_reject(request, reject);
            return;
        }
    };
    let result = config.client.update_order(order_id, &params);
    respond_result(request, "order update", result);
}

/// Cancel carries no body.
fn handle_cancel_order(request: Request, config: &Config, order_id: &str) {
    let result = config.client.cancel_order(order_id);
    respond_result(request, "order cancel", result);
}

fn handle_create_fulfillment(mut request: Request, config: &Config, order_id: &str) {
    let params = match read_json_object(&mut request) {
        Ok(v) => v,
        Err(reject) => {
            respond_reject(request, reject);
            return;
        }
    };
    let result = config.client.create_fulfillment(order_id, &params);
    respond_result(request, "fulfillment", result);
}

fn handle_update_fulfillment(
    mut request: Request,
    config: &Config,
    order_id: &str,
    fulfillment_id: &str,
) {
    let params = match read_json_object(&mut request) {
        Ok(v) => v,
        Err(reject) => {
            respond_reject(request, reject);
            return;
        }
    };
    let result = config
        .client
        .update_fulfillment(order_id, fulfillment_id, &params);
    respond_result(request, "fulfillment update", result);
}

fn handle_webhook(mut request: Request, config: &Config) {
    // An unreadable body can't be verified, so treat it as unsigned. An
    // oversize one is refused like any other route's.
    let raw = match read_body(&mut request) {
        Ok(b) => b,
        Err(reject) if reject.status == 413 => {
            respond_reject(request, reject);
            return;
        }
        Err(_) => {
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
    // An empty secret is a valid HMAC key, so without this check an
    // unconfigured server authenticates anyone who signs with "".
    if config.webhook_secret.is_empty()
        || !verify_webhook_signature(&raw, &signature, &config.webhook_secret)
    {
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
    if method == Method::Options {
        respond_preflight(request);
        return;
    }
    if method != Method::Post {
        respond_json(request, 404, &json!({"error": "not found"}));
        return;
    }

    let route = parse_route(&url);
    // The webhook route is authenticated by its HMAC signature instead.
    if !matches!(route, Route::Webhook) && !authenticate_caller(&request) {
        respond_json(request, 401, &json!({"error": "unauthorized"}));
        return;
    }

    match route {
        Route::Quote => handle_quote(request, config),
        Route::Webhook => handle_webhook(request, config),
        Route::CreateOrder => handle_create_order(request, config),
        Route::UpdateOrder(id) => handle_update_order(request, config, &id),
        Route::CancelOrder(id) => handle_cancel_order(request, config, &id),
        Route::CreateFulfillment(id) => handle_create_fulfillment(request, config, &id),
        Route::UpdateFulfillment(id, fid) => {
            handle_update_fulfillment(request, config, &id, &fid)
        }
        Route::BadPathParam => {
            respond_json(request, 400, &json!({"error": "invalid order id in path"}))
        }
        Route::NotFound => respond_json(request, 404, &json!({"error": "not found"})),
    }
}

fn main() {
    // Loopback by default: this demo authenticates nobody, so it must not
    // be reachable from the network unless the operator asks for that.
    let host = env_or("HOST", "127.0.0.1");
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

    let server = match Server::http((host.as_str(), port)) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("failed to bind {host}:{port}: {err}");
            std::process::exit(1);
        }
    };
    println!("listening on http://{host}:{port}");

    // Handle each request on its own thread so webhook deliveries do not
    // queue behind each other.
    for request in server.incoming_requests() {
        let config = Arc::clone(&config);
        thread::spawn(move || handle_request(request, &config));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seel_direct_integration_reference::SeelApiError;
    use std::io::Cursor;

    #[test]
    fn body_cap_refuses_declared_length_without_reading() {
        let mut never_read = Cursor::new(vec![b'a'; 8]);
        let reject = read_capped(Some(MAX_BODY_BYTES + 1), &mut never_read).err().unwrap();
        assert_eq!(reject.status, 413);
        assert_eq!(reject.error, "request body exceeds 1048576 bytes");
        assert_eq!(never_read.position(), 0);
        assert!(read_capped(Some(MAX_BODY_BYTES), &mut Cursor::new(vec![])).is_ok());
    }

    /// A chunked body declares no length, so the cap has to hold while
    /// reading, and stop reading once it is passed.
    #[test]
    fn body_cap_holds_while_reading_an_undeclared_length() {
        let mut body = Cursor::new(vec![b'a'; 2 * MAX_BODY_BYTES]);
        let reject = read_capped(None, &mut body).err().unwrap();
        assert_eq!(reject.status, 413);
        assert_eq!(body.position() as usize, MAX_BODY_BYTES + 1);

        let exact = vec![b'a'; MAX_BODY_BYTES];
        assert_eq!(read_capped(None, &mut Cursor::new(exact.clone())).unwrap(), exact);
    }

    #[test]
    fn validation_is_a_400_with_every_problem() {
        let (status, body) = map_result(
            "quote",
            Err(SeelError::Validation {
                operation: "create_quote".to_string(),
                problems: vec!["missing required field a".to_string(), "b must be an object".to_string()],
            }),
        );
        assert_eq!(status, 400);
        assert_eq!(body["error"], "create_quote: missing required field a; b must be an object");
        assert_eq!(body["problems"], json!(["missing required field a", "b must be an object"]));

        let (status, body) = map_result(
            "order cancel",
            Err(SeelError::InvalidId {
                operation: "cancel_order".to_string(),
                problem: "order_id must not be empty".to_string(),
            }),
        );
        assert_eq!(status, 400);
        assert_eq!(body["problems"], json!(["order_id must not be empty"]));
    }

    #[test]
    fn unminted_contract_is_a_409_carrying_seels_response() {
        let seel = json!({"seel_services": [{"type": "x", "contract_id": null}]});
        let (status, body) = map_result(
            "order",
            Err(SeelError::ContractNotMinted {
                detail: "service x returned contract_id=null".to_string(),
                response: Box::new(seel.clone()),
            }),
        );
        assert_eq!(status, 409);
        assert_eq!(
            body["error"],
            "order created but no contract was minted: service x returned contract_id=null"
        );
        assert_eq!(body["seel_response"], seel);
    }

    /// Seel's own status and body travel through untouched, for errors and
    /// for a 2xx whose body was not JSON. A non-object body is wrapped so
    /// the caller always gets a JSON object.
    #[test]
    fn seel_status_passes_through() {
        let api = |status, body| Err(SeelError::Api(SeelApiError { status, body }));
        let (status, body) = map_result("quote", api(422, json!({"error": "bad field", "trace_id": "t"})));
        assert_eq!((status, body), (422, json!({"error": "bad field", "trace_id": "t"})));

        let (status, body) = map_result("order update", api(201, json!({"seel_raw_body": "ok"})));
        assert_eq!((status, body), (201, json!({"seel_raw_body": "ok"})));

        let (status, body) = map_result("quote", api(500, json!("<html>oops</html>")));
        assert_eq!(status, 500);
        assert_eq!(body, json!({"error": "Seel API 500: <html>oops</html>"}));
    }

    #[test]
    fn no_response_is_a_502_and_timeout_is_a_504() {
        let no_response = SeelError::Transport(Box::new(ureq::Error::from(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset"),
        )));
        let (status, body) = map_result("quote", Err(no_response));
        assert_eq!(status, 502);
        assert_eq!(body, json!({"error": "upstream quote request failed"}));

        let timed_out = SeelError::Transport(Box::new(ureq::Error::from(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out reading response"),
        )));
        let (status, body) = map_result("order", Err(timed_out));
        assert_eq!(status, 504);
        assert_eq!(
            body["error"],
            "timed out waiting for Seel; the request may have been processed, check before retrying"
        );

        let body_timed_out = SeelError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "slow body"));
        assert_eq!(map_result("order", Err(body_timed_out)).0, 504);
    }
}
