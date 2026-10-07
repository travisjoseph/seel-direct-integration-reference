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

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Body, Bytes, Incoming};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use seel_direct_integration_reference::{
    parse_route, verify_webhook_signature, Route, SeelClient, SeelError, SANDBOX_BASE_URL,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

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

type Reply = Response<Full<Bytes>>;

fn json_response(status: u16, body: &Value) -> Reply {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        // Demo only; lock down in prod.
        .header("Access-Control-Allow-Origin", "*")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_else(|err| unreachable!("static headers and a u16 status are always well-formed: {err}"))
}

/// CORS preflight for the demo page.
fn preflight_response() -> Reply {
    Response::builder()
        .status(204)
        // Demo only; lock down in prod.
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Headers", "Content-Type")
        .header("Access-Control-Allow-Methods", "POST, OPTIONS")
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|err| unreachable!("static headers are always well-formed: {err}"))
}

/// The largest request body any route accepts. A quote or order payload is
/// a few KB; without a cap one 200 MB POST sits in this process's memory.
const MAX_BODY_BYTES: usize = 1 << 20;

/// How long a client gets to finish sending its headers, and then its body.
/// A client that trickles bytes forever would otherwise hold its connection
/// task open for as long as it likes.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a request body was not accepted, with the status it answers.
#[derive(Debug)]
struct BodyReject {
    status: u16,
    error: String,
    /// Part of the body is still in the socket. The connection has to
    /// close then: hyper would otherwise parse what remains as the next
    /// request, which is how a request hidden inside an oversized chunk
    /// could run.
    unread: bool,
}

impl BodyReject {
    fn too_large() -> Self {
        BodyReject {
            status: 413,
            error: format!("request body exceeds {MAX_BODY_BYTES} bytes"),
            unread: true,
        }
    }

    fn unreadable() -> Self {
        BodyReject { status: 400, error: "could not read request body".to_string(), unread: true }
    }

    fn timed_out() -> Self {
        BodyReject {
            status: 408,
            error: format!("request body not received within {}s", BODY_READ_TIMEOUT.as_secs()),
            unread: true,
        }
    }

    /// The body arrived in full and is not usable.
    fn bad_request(error: &str) -> Self {
        BodyReject { status: 400, error: error.to_string(), unread: false }
    }

    fn into_response(self) -> Reply {
        let mut response = json_response(self.status, &json!({ "error": self.error }));
        if self.unread {
            response.headers_mut().insert("Connection", "close".parse().expect("static header value"));
        }
        response
    }
}

/// Read a body of at most `MAX_BODY_BYTES`. A declared Content-Length over
/// the cap is refused up front so nothing is read; a chunked or lying body
/// is cut off while reading.
async fn read_capped<B>(body: B) -> Result<Vec<u8>, BodyReject>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if body.size_hint().lower() > MAX_BODY_BYTES as u64 {
        return Err(BodyReject::too_large());
    }
    match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => Ok(collected.to_bytes().to_vec()),
        Err(err) if err.is::<LengthLimitError>() => Err(BodyReject::too_large()),
        Err(_) => Err(BodyReject::unreadable()),
    }
}

async fn read_body(body: Incoming) -> Result<Vec<u8>, BodyReject> {
    match tokio::time::timeout(BODY_READ_TIMEOUT, read_capped(body)).await {
        Ok(result) => result,
        Err(_) => Err(BodyReject::timed_out()),
    }
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
fn authenticate_caller(_head: &Parts) -> bool {
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

/// Run one blocking client call on tokio's blocking pool, so a slow Seel
/// round trip never stalls the connection tasks sharing this runtime.
async fn call_seel<F>(config: &Arc<Config>, call: F) -> Result<Value, SeelError>
where
    F: FnOnce(&SeelClient) -> Result<Value, SeelError> + Send + 'static,
{
    let config = Arc::clone(config);
    match tokio::task::spawn_blocking(move || call(&config.client)).await {
        Ok(result) => result,
        Err(err) => Err(SeelError::Io(std::io::Error::other(format!("client call aborted: {err}")))),
    }
}

fn result_response(label: &str, result: Result<Value, SeelError>) -> Reply {
    let (status, body) = map_result(label, result);
    json_response(status, &body)
}

/// Read and parse a JSON object body, or return the rejection to send back.
async fn read_json_object(body: Incoming) -> Result<Value, BodyReject> {
    // A body that never fully arrived is not the same as a malformed one,
    // and saying so would send the caller looking at the wrong thing.
    let raw = read_body(body).await?;
    let parsed: Value = serde_json::from_slice(&raw)
        .map_err(|_| BodyReject::bad_request("request body must be JSON"))?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(BodyReject::bad_request("request body must be a JSON object"))
    }
}

async fn handle_quote(body: Incoming, config: &Arc<Config>) -> Reply {
    let mut params = match read_json_object(body).await {
        Ok(v) => v,
        Err(reject) => return reject.into_response(),
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
    let result = call_seel(config, move |client| client.create_quote(&params)).await;
    result_response("quote", result)
}

/// Sync every order, opted in or not. On opt-in the body carries
/// seel_services with the quote_id and price, which mints the contract and
/// fires contract.created.
async fn handle_create_order(body: Incoming, config: &Arc<Config>) -> Reply {
    let mut params = match read_json_object(body).await {
        Ok(v) => v,
        Err(reject) => return reject.into_response(),
    };
    let merchant_id = resolve_merchant_id(config, &params);
    if let Some(obj) = params.as_object_mut() {
        if !merchant_id.is_empty() {
            obj.insert("merchant_id".to_string(), Value::String(merchant_id));
        }
    }
    let result = call_seel(config, move |client| client.create_order(&params)).await;
    result_response("order", result)
}

async fn handle_update_order(body: Incoming, config: &Arc<Config>, order_id: String) -> Reply {
    let params = match read_json_object(body).await {
        Ok(v) => v,
        Err(reject) => return reject.into_response(),
    };
    let result = call_seel(config, move |client| client.update_order(&order_id, &params)).await;
    result_response("order update", result)
}

/// Cancel carries no body.
async fn handle_cancel_order(config: &Arc<Config>, order_id: String) -> Reply {
    let result = call_seel(config, move |client| client.cancel_order(&order_id)).await;
    result_response("order cancel", result)
}

async fn handle_create_fulfillment(body: Incoming, config: &Arc<Config>, order_id: String) -> Reply {
    let params = match read_json_object(body).await {
        Ok(v) => v,
        Err(reject) => return reject.into_response(),
    };
    let result =
        call_seel(config, move |client| client.create_fulfillment(&order_id, &params)).await;
    result_response("fulfillment", result)
}

async fn handle_update_fulfillment(
    body: Incoming,
    config: &Arc<Config>,
    order_id: String,
    fulfillment_id: String,
) -> Reply {
    let params = match read_json_object(body).await {
        Ok(v) => v,
        Err(reject) => return reject.into_response(),
    };
    let result = call_seel(config, move |client| {
        client.update_fulfillment(&order_id, &fulfillment_id, &params)
    })
    .await;
    result_response("fulfillment update", result)
}

async fn handle_webhook(head: &Parts, body: Incoming, config: &Arc<Config>) -> Reply {
    // An unreadable body can't be verified, so treat it as unsigned. An
    // oversize or stalled one is refused like any other route's.
    let raw = match read_body(body).await {
        Ok(b) => b,
        Err(reject) if reject.status == 400 => {
            return BodyReject { status: 401, error: "invalid signature".to_string(), ..reject }
                .into_response()
        }
        Err(reject) => return reject.into_response(),
    };
    let signature = head
        .headers
        .get("X-Seel-Hmac-SHA256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // An empty secret is a valid HMAC key, so without this check an
    // unconfigured server authenticates anyone who signs with "".
    if config.webhook_secret.is_empty()
        || !verify_webhook_signature(&raw, signature, &config.webhook_secret)
    {
        return json_response(401, &json!({"error": "invalid signature"}));
    }
    // ACK before doing any work: Seel retries anything not answered with a
    // 200 within 10 seconds, so processing runs on its own task, where it
    // can neither delay nor fail the response.
    tokio::task::spawn_blocking(move || {
        match serde_json::from_slice::<Value>(&raw) {
            Ok(event) => handle_webhook_event(&event),
            Err(err) => eprintln!("[webhook] processing error: {err}"),
        }
    });
    json_response(200, &json!({"ok": true}))
}

async fn handle_request(request: Request<Incoming>, config: Arc<Config>) -> Result<Reply, Infallible> {
    let (head, body) = request.into_parts();
    if head.method == Method::OPTIONS {
        return Ok(preflight_response());
    }
    if head.method != Method::POST {
        return Ok(json_response(404, &json!({"error": "not found"})));
    }

    let route = parse_route(head.uri.path());
    // The webhook route is authenticated by its HMAC signature instead.
    if !matches!(route, Route::Webhook) && !authenticate_caller(&head) {
        return Ok(json_response(401, &json!({"error": "unauthorized"})));
    }

    Ok(match route {
        Route::Quote => handle_quote(body, &config).await,
        Route::Webhook => handle_webhook(&head, body, &config).await,
        Route::CreateOrder => handle_create_order(body, &config).await,
        Route::UpdateOrder(id) => handle_update_order(body, &config, id).await,
        Route::CancelOrder(id) => handle_cancel_order(&config, id).await,
        Route::CreateFulfillment(id) => handle_create_fulfillment(body, &config, id).await,
        Route::UpdateFulfillment(id, fid) => {
            handle_update_fulfillment(body, &config, id, fid).await
        }
        Route::BadPathParam => json_response(400, &json!({"error": "invalid order id in path"})),
        Route::NotFound => json_response(404, &json!({"error": "not found"})),
    })
}

async fn serve(listener: TcpListener, config: Arc<Config>) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                eprintln!("accept failed: {err}");
                continue;
            }
        };
        let config = Arc::clone(&config);
        // One task per connection, so webhook deliveries do not queue
        // behind each other.
        tokio::spawn(async move {
            let service = service_fn(move |request| handle_request(request, Arc::clone(&config)));
            let served = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await;
            // A client that stops mid-request or idles past the header
            // timeout is routine, not a fault worth a log line.
            if let Err(err) = served {
                if !err.is_incomplete_message() && !err.is_timeout() {
                    eprintln!("connection error: {err}");
                }
            }
        });
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

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(err) => {
            eprintln!("failed to start runtime: {err}");
            std::process::exit(1);
        }
    };
    runtime.block_on(async {
        let listener = match TcpListener::bind((host.as_str(), port)).await {
            Ok(l) => l,
            Err(err) => {
                eprintln!("failed to bind {host}:{port}: {err}");
                std::process::exit(1);
            }
        };
        println!("listening on http://{host}:{port}");
        serve(listener, config).await
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::body::{Frame, SizeHint};
    use seel_direct_integration_reference::SeelApiError;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    /// A body that serves fixed frames and counts how many were taken, with
    /// a declared length that may lie, like a Content-Length header can.
    struct FakeBody {
        declared: Option<u64>,
        frames: VecDeque<Bytes>,
        polled: Arc<AtomicUsize>,
    }

    impl Body for FakeBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            self.polled.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(self.frames.pop_front().map(|b| Ok(Frame::data(b))))
        }

        fn size_hint(&self) -> SizeHint {
            self.declared.map(SizeHint::with_exact).unwrap_or_default()
        }
    }

    fn fake(declared: Option<u64>, frames: Vec<Bytes>) -> (FakeBody, Arc<AtomicUsize>) {
        let polled = Arc::new(AtomicUsize::new(0));
        (FakeBody { declared, frames: frames.into(), polled: Arc::clone(&polled) }, polled)
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    #[test]
    fn body_cap_refuses_declared_length_without_reading() {
        let (never_read, polled) = fake(Some(MAX_BODY_BYTES as u64 + 1), vec![Bytes::from("a")]);
        let reject = block_on(read_capped(never_read)).err().unwrap();
        assert_eq!(reject.status, 413);
        assert_eq!(reject.error, "request body exceeds 1048576 bytes");
        assert_eq!(polled.load(Ordering::SeqCst), 0);

        let (exact, _) = fake(Some(MAX_BODY_BYTES as u64), vec![Bytes::from(vec![b'a'; MAX_BODY_BYTES])]);
        assert!(block_on(read_capped(exact)).is_ok());
    }

    /// A chunked body declares no length, so the cap has to hold while
    /// reading, and stop reading once it is passed.
    #[test]
    fn body_cap_holds_while_reading_an_undeclared_length() {
        let frame = Bytes::from(vec![b'a'; MAX_BODY_BYTES / 2]);
        let (body, polled) = fake(None, vec![frame.clone(); 4]);
        let reject = block_on(read_capped(body)).err().unwrap();
        assert_eq!(reject.status, 413);
        assert_eq!(polled.load(Ordering::SeqCst), 3, "stops at the first frame past the cap");

        let (body, _) = fake(None, vec![frame.clone(); 2]);
        assert_eq!(block_on(read_capped(body)).unwrap(), vec![b'a'; MAX_BODY_BYTES]);
    }

    /// Only a reject that leaves bytes in the socket closes the connection.
    #[test]
    fn only_an_unread_body_closes_the_connection() {
        for reject in [BodyReject::too_large(), BodyReject::unreadable(), BodyReject::timed_out()] {
            let status = reject.status;
            let response = reject.into_response();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()["Connection"], "close", "{status}");
            assert_eq!(response.headers()["Content-Type"], "application/json");
        }
        let response = BodyReject::bad_request("request body must be JSON").into_response();
        assert_eq!(response.status(), 400);
        assert!(response.headers().get("Connection").is_none());
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
