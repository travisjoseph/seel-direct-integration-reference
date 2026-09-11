//! Rust client for Seel's ecommerce APIs (the direct-integration path).
//!
//! Wraps the public /v1/ecommerce/* APIs and follows the flow in the SaaS
//! Platform Integration Quickstart
//! (<https://developer.seel.com/docs/platform-direct-integration>):
//!
//! ```text
//! merchant enables program -> create_merchant (+ batch order-history backfill)
//! checkout                 -> create_quote (widget renders from the response)
//! order placed             -> create_order (all orders, opted in or not;
//!                             seel_services carries quote_id + price on opt-in)
//! order shipped/delivered  -> create_fulfillment / update_fulfillment
//! order changed/cancelled  -> update_order / cancel_order
//! return or claim filed    -> create_claim (+ update_claim with the decision
//!                             when the platform adjudicates)
//! async status changes     -> webhooks (contract.*, claim.*), HMAC-signed
//! ```
//!
//! Blocking, a few small crates, no async runtime. Written to be ported.

use std::fmt;
use std::time::Duration;

use base64::prelude::*;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// Percent-encode one path segment.
///
/// Ids come from callers and go straight into the upstream URL. Without
/// this, an id containing `/` (or `%2F`, which decodes to one) reaches a
/// different endpoint than the method name implies: an `update_order` call
/// with order id `x/cancel` would cancel instead.
pub fn path_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Percent-decode one path segment, or return None if it escapes its
/// segment.
///
/// A decoded `/` would reach a different endpoint than the route implies,
/// and control characters are refused for the same reason. A malformed
/// escape is rejected rather than silently substituted, so every port
/// answers the same way. With one API key shared across retailers this is a
/// privilege boundary, not a cosmetic check.
/// Segments that are Seel endpoints in their own right and so can never be
/// an order id. Seel's own collection endpoints live alongside order ids,
/// so an id that equals one of them would reach the collection instead.
/// `batch` is `POST /v1/ecommerce/orders/batch`, the order-history
/// backfill: routed as an order id it would proxy an unstamped,
/// unvalidated batch write.
pub const RESERVED_PATH_SEGMENTS: &[&str] = &["batch"];

pub fn safe_path_param(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
            let byte = hex.and_then(|h| u8::from_str_radix(h, 16).ok())?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    if decoded.contains('/') || decoded.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7F) {
        return None;
    }
    if RESERVED_PATH_SEGMENTS.contains(&decoded.as_str()) {
        return None;
    }
    Some(decoded)
}

/// The routes the example server answers.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Quote,
    Webhook,
    CreateOrder,
    UpdateOrder(String),
    CancelOrder(String),
    CreateFulfillment(String),
    UpdateFulfillment(String, String),
    /// A path that matched a route shape but carried an id that escapes its
    /// segment.
    BadPathParam,
    NotFound,
}

/// Match a URL onto a route.
///
/// The query string is stripped and segments are compared exactly. Empty
/// segments are NOT collapsed: `/orders//cancel` must not resolve to the
/// update-order route with an id of `cancel`.
pub fn parse_route(url: &str) -> Route {
    let path = url.split('?').next().unwrap_or(url);
    let segments: Vec<&str> = path.split('/').collect();
    // A leading '/' always yields an empty first segment.
    let segments = match segments.split_first() {
        Some((first, rest)) if first.is_empty() => rest,
        _ => return Route::NotFound,
    };
    // An empty id segment is not a match at all, mirroring the `([^/]+)`
    // regexes the Python and Node ports use: `/orders//cancel` is a 404,
    // not a cancel of an order called "".
    let decode = |raw: &str| if raw.is_empty() { None } else { safe_path_param(raw) };
    let empty_id = |raw: &str| raw.is_empty();
    match segments {
        ["v1", "ecommerce", "quotes"] => Route::Quote,
        ["webhooks", "seel"] => Route::Webhook,
        ["v1", "ecommerce", "orders"] => Route::CreateOrder,
        ["v1", "ecommerce", "orders", id] if !empty_id(id) => match decode(id) {
            Some(id) => Route::UpdateOrder(id),
            None => Route::BadPathParam,
        },
        ["v1", "ecommerce", "orders", id, "cancel"] if !empty_id(id) => match decode(id) {
            Some(id) => Route::CancelOrder(id),
            None => Route::BadPathParam,
        },
        ["v1", "ecommerce", "orders", id, "fulfillments"] if !empty_id(id) => match decode(id) {
            Some(id) => Route::CreateFulfillment(id),
            None => Route::BadPathParam,
        },
        ["v1", "ecommerce", "orders", id, "fulfillments", fid]
            if !empty_id(id) && !empty_id(fid) =>
        {
            match (decode(id), decode(fid)) {
                (Some(id), Some(fid)) => Route::UpdateFulfillment(id, fid),
                _ => Route::BadPathParam,
            }
        }
        _ => Route::NotFound,
    }
}

pub const SANDBOX_BASE_URL: &str = "https://api-test.seel.com";
pub const PRODUCTION_BASE_URL: &str = "https://api.seel.com";
/// The pinned API version; all four language ports match.
pub const API_VERSION: &str = "2.6.0";

/// Returned on any non-2xx response. Carries the status and Seel's JSON
/// error body: the message names the offending field, and the trace_id is
/// what Seel support will ask for.
#[derive(Debug)]
pub struct SeelApiError {
    pub status: u16,
    /// Seel's JSON error body, or the raw text if it wasn't valid JSON.
    pub body: Value,
}

impl fmt::Display for SeelApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.body.get("error") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => match &self.body {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
        };
        write!(f, "Seel API {}: {}", self.status, message)
    }
}

impl std::error::Error for SeelApiError {}

/// Everything a client call can fail with: a Seel-reported API error, or a
/// failure before/while reading a response.
#[derive(Debug)]
pub enum SeelError {
    /// Non-2xx response from Seel (see [`SeelApiError`]).
    Api(SeelApiError),
    /// Network or protocol failure before a usable response was received.
    Transport(Box<ureq::Error>),
    /// Failed to read or decode a response body.
    Io(std::io::Error),
    /// The payload was rejected before any request was made: missing fields
    /// Seel requires, or a shape the API accepts and then fails on. Carries
    /// every problem at once rather than one 400 at a time.
    Validation { operation: String, problems: Vec<String> },
    /// `create_order` returned 200 but no contract was created. Every
    /// failed attach observed so far is `contract_id: null` on an otherwise
    /// successful response rather than a status code, so without this check
    /// an integration looks healthy while covering nothing.
    ContractNotMinted { detail: String, response: Box<Value> },
}

impl fmt::Display for SeelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SeelError::Api(err) => write!(f, "{err}"),
            SeelError::Transport(err) => write!(f, "transport error: {err}"),
            SeelError::Io(err) => write!(f, "response read error: {err}"),
            SeelError::Validation { operation, problems } => write!(
                f,
                "{operation}: {}. Call .without_validation() on the client to skip these checks.",
                problems.join("; ")
            ),
            SeelError::ContractNotMinted { detail, .. } => {
                write!(f, "order created but no contract was minted: {detail}")
            }
        }
    }
}

impl std::error::Error for SeelError {}

// Required-field sets, measured against sandbox on 2026-09-10 by removing
// one field per request from a known-good payload and recording the
// response.
//
// Treat these as a starting point, not a fixed contract. A newly
// provisioned account behaves this way; as an integration develops, Seel's
// implementation team works out which fields a merchant journey can
// actually supply and eases the validation accordingly, so an established
// account may accept less. Sending the full set is never wrong, but
// rejecting a payload locally could be, which is why validation is
// advisory and can be turned off.
//
// A "[]" suffix means the rule applies to every element of that array.
const LINE_ITEM_REQUIRED: &[&str] = &[
    "line_item_id", "product_id", "product_title", "quantity", "price",
    "allocated_discounts", "sales_tax", "final_price", "currency",
    "requires_shipping", "image_urls", "category_1", "category_2",
    "is_final_sale", "shipping_origin",
];

const QUOTE_REQUIRED: &[(&str, &[&str])] = &[
    ("", &["merchant_id", "session_id", "device_category", "device_platform", "type",
           "is_default_on", "customer", "shipping_address", "line_items"]),
    ("customer", &["customer_id", "email"]),
    ("shipping_address", &["address_1", "city", "state", "zipcode", "country"]),
    ("line_items[]", LINE_ITEM_REQUIRED),
    ("line_items[].shipping_origin", &["country"]),
];

const ORDER_REQUIRED: &[(&str, &[&str])] = &[
    ("", &["merchant_id", "order_id", "order_number", "created_ts", "session_id",
           "device_category", "device_platform", "customer", "shipping_address", "line_items"]),
    ("customer", &["customer_id", "email"]),
    ("shipping_address", &["address_1", "city", "state", "zipcode", "country"]),
    ("line_items[]", LINE_ITEM_REQUIRED),
    ("line_items[].shipping_origin", &["country"]),
];

/// Only checked when `seel_services` is present - an order with no coverage
/// is a normal sync, not an error.
const ORDER_SERVICE_REQUIRED: &[(&str, &[&str])] =
    &[("seel_services[]", &["type", "quote_id", "price"])];

/// `coverages` must be PRESENT but may be an empty array. Omitting it
/// returns a 500 rather than a validation error, so catching it locally is
/// the whole point of validating this call.
const MERCHANT_REQUIRED: &[(&str, &[&str])] = &[
    ("", &["shop_id", "admin_domain", "shop_domain", "shop_platform", "shop_currency",
           "shop_name", "contact_name", "contact_email", "seel_services"]),
    ("seel_services[]", &["type", "coverages"]),
];

/// Missing means the key is absent, null, or an empty string. `false` and
/// `0` are real values - `is_default_on`, `requires_shipping` and
/// `allocated_discounts` all legitimately take them. An empty array is a
/// real value too: merchant `coverages: []` is accepted.
fn is_absent(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

/// Collect `(node, dotted_prefix)` pairs a scope selects.
fn resolve_scope<'a>(payload: &'a Value, scope: &str) -> Vec<(&'a Value, String)> {
    let mut nodes: Vec<(&Value, String)> = vec![(payload, String::new())];
    if scope.is_empty() {
        return nodes;
    }
    for part in scope.split('.') {
        let (key, fan_out) = match part.strip_suffix("[]") {
            Some(k) => (k, true),
            None => (part, false),
        };
        let mut next = Vec::new();
        for (node, prefix) in &nodes {
            let Some(child) = node.get(key) else { continue };
            if fan_out {
                if let Some(items) = child.as_array() {
                    for (i, item) in items.iter().enumerate() {
                        if item.is_object() {
                            next.push((item, format!("{prefix}{key}[{i}].")));
                        }
                    }
                }
            } else if child.is_object() {
                next.push((child, format!("{prefix}{key}.")));
            }
        }
        nodes = next;
    }
    nodes
}

fn collect_missing(payload: &Value, rules: &[(&str, &[&str])]) -> Vec<String> {
    let mut missing = Vec::new();
    for (scope, fields) in rules {
        for (node, prefix) in resolve_scope(payload, scope) {
            for field in *fields {
                if is_absent(node.get(field)) {
                    missing.push(format!("{prefix}{field}"));
                }
            }
        }
    }
    missing
}

/// Shape expectations, checked alongside presence. A scalar where an
/// object belongs is the archetypal payload mistake, and without this the
/// nested rules silently skip it: `resolve_scope` only descends into
/// objects, so `{"customer": "nope"}` would report no problems at all.
///
/// Each entry is (parent scope, key, kind).
const QUOTE_SHAPES: &[(&str, &str, &str)] = &[
    ("", "customer", "object"),
    ("", "shipping_address", "object"),
    ("", "line_items", "array_nonempty"),
    ("line_items[]", "shipping_origin", "object"),
];
const ORDER_EXTRA_SHAPES: &[(&str, &str, &str)] = &[("", "seel_services", "array")];
const MERCHANT_SHAPES: &[(&str, &str, &str)] = &[("", "seel_services", "array_nonempty")];

/// One vocabulary for type names across all four ports, so the same
/// mistake reads the same way whichever one a partner runs.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::Null => "null",
    }
}

fn check_shapes(payload: &Value, specs: &[(&str, &str, &str)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (scope, key, kind) in specs {
        for (node, prefix) in resolve_scope(payload, scope) {
            let Some(value) = node.get(key) else { continue };
            if value.is_null() {
                continue; // absence is the required-field check's job
            }
            let path = format!("{prefix}{key}");
            if *kind == "object" && !value.is_object() {
                problems.push(format!("{path} must be an object, got {}", type_name(value)));
            } else if kind.starts_with("array") {
                match value.as_array() {
                    None => problems
                        .push(format!("{path} must be an array, got {}", type_name(value))),
                    Some(items) if *kind == "array_nonempty" && items.is_empty() => {
                        problems.push(format!("{path} must not be empty"))
                    }
                    Some(_) => {}
                }
            }
        }
    }
    problems
}

fn as_problems(missing: Vec<String>) -> Vec<String> {
    missing
        .into_iter()
        .map(|f| format!("missing required field {f}"))
        .collect()
}

/// Return the problems with a Create Quote payload.
pub fn validate_quote_payload(payload: &Value) -> Vec<String> {
    let mut problems = as_problems(collect_missing(payload, QUOTE_REQUIRED));
    problems.extend(check_shapes(payload, QUOTE_SHAPES));
    problems
}

/// Return the problems with a Create Order payload.
///
/// `seel_services` is only checked for completeness when present. Two shape
/// mistakes are checked separately, because the API accepts both and then
/// fails in ways that do not look like failures.
pub fn validate_order_payload(payload: &Value) -> Vec<String> {
    let services = payload.get("seel_services");
    let mut rules: Vec<(&str, &[&str])> = ORDER_REQUIRED.to_vec();
    if services.map(Value::is_array).unwrap_or(false)
        && !services.and_then(Value::as_array).map(Vec::is_empty).unwrap_or(true)
    {
        rules.extend_from_slice(ORDER_SERVICE_REQUIRED);
    }
    let mut problems = as_problems(collect_missing(payload, &rules));
    problems.extend(check_shapes(payload, QUOTE_SHAPES));
    problems.extend(check_shapes(payload, ORDER_EXTRA_SHAPES));

    // Create Order has no top-level quote_id. Sending one is the classic
    // attach mistake: the API returns 200 with seel_services: null and no
    // error, so the integration looks healthy while covering nothing.
    if payload.get("quote_id").is_some() {
        problems.push(
            "quote_id must go inside a seel_services entry, not at the top level - a \
             top-level quote_id is ignored and the order attaches no coverage"
                .to_string(),
        );
    }
    problems
}

/// Return the problems with a Create Merchant payload.
pub fn validate_merchant_payload(payload: &Value) -> Vec<String> {
    let mut problems = as_problems(collect_missing(payload, MERCHANT_REQUIRED));
    problems.extend(check_shapes(payload, MERCHANT_SHAPES));
    problems
}

pub struct SeelClient {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
    api_version: String,
    /// Pre-flight payload validation against the strict profile.
    validate: bool,
    /// Post-condition check on `create_order`: did a contract actually mint?
    check_contract: bool,
}

impl SeelClient {
    /// Build a client against `base_url` with the pinned [`API_VERSION`] and
    /// a 15 second request timeout.
    pub fn new(api_key: &str, base_url: &str) -> Self {
        Self::with_options(api_key, base_url, API_VERSION, Duration::from_secs(15))
    }

    pub fn with_options(
        api_key: &str,
        base_url: &str,
        api_version: &str,
        timeout: Duration,
    ) -> Self {
        SeelClient {
            agent: ureq::AgentBuilder::new().timeout(timeout).build(),
            api_key: api_key.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_version: api_version.to_string(),
            validate: true,
            check_contract: true,
        }
    }

    /// Turn pre-flight validation off. Do this for an account Seel has
    /// relaxed required fields for, or to let the API be the only
    /// authority on what a valid payload is.
    ///
    /// This leaves the `create_order` contract check on, because that
    /// reports a real failure the API returns as a 200 rather than an
    /// opinion about required fields. Use
    /// [`Self::without_contract_check`] to drop that too.
    #[must_use]
    pub fn without_validation(mut self) -> Self {
        self.validate = false;
        self
    }

    /// Stop checking that `create_order` actually minted a contract.
    ///
    /// Only do this if you check `seel_services[].contract_id` yourself. A
    /// failed attach is reported as `contract_id: null` on a 200, so
    /// nothing else in the stack will notice.
    #[must_use]
    pub fn without_contract_check(mut self) -> Self {
        self.check_contract = false;
        self
    }

    fn check(&self, operation: &str, problems: Vec<String>) -> Result<(), SeelError> {
        if self.validate && !problems.is_empty() {
            return Err(SeelError::Validation {
                operation: operation.to_string(),
                problems,
            });
        }
        Ok(())
    }

    /// Fail loudly when an attach silently did not take. A failed attach is
    /// `contract_id: null` on a 200, never a status code, so nothing else
    /// in the stack will notice.
    fn check_contract_minted(payload: &Value, response: &Value) -> Result<(), SeelError> {
        let sent = payload
            .get("seel_services")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let services = response.get("seel_services").and_then(Value::as_array);
        let Some(services) = services.filter(|s| !s.is_empty()) else {
            return Err(SeelError::ContractNotMinted {
                detail: format!(
                    "sent {sent} seel_services entr{}, response seel_services is {}. Check \
                     seel_services is an array and quote_id is inside it, not at the top level.",
                    if sent == 1 { "y" } else { "ies" },
                    response.get("seel_services").unwrap_or(&Value::Null)
                ),
                response: Box::new(response.clone()),
            });
        };
        for entry in services {
            // Falsy means not minted, matching Python and Node: null, "",
            // 0 and false all mean no contract.
            let minted = entry.get("contract_id").is_some_and(|c| match c {
                Value::Null => false,
                Value::String(s) => !s.is_empty(),
                Value::Number(n) => n.as_f64() != Some(0.0),
                Value::Bool(b) => *b,
                _ => true,
            });
            if minted {
                continue;
            }
            return Err(SeelError::ContractNotMinted {
                detail: format!(
                    "service {} returned contract_id=null (status={}, error={})",
                    entry.get("type").unwrap_or(&Value::Null),
                    entry.get("status").unwrap_or(&Value::Null),
                    entry.get("error").unwrap_or(&Value::Null)
                ),
                response: Box::new(response.clone()),
            });
        }
        Ok(())
    }

    fn request(&self, method: &str, path: &str, payload: Option<&Value>) -> Result<Value, SeelError> {
        let url = format!("{}/v1{}", self.base_url, path);
        // Header names are case-insensitive per RFC 9110; Seel accepts any
        // casing on the wire.
        let req = self
            .agent
            .request(method, &url)
            .set("X-Seel-Api-Key", &self.api_key)
            .set("X-Seel-Api-Version", &self.api_version)
            .set("Content-Type", "application/json");
        let result = match payload {
            Some(p) => req.send_json(p),
            None => req.call(),
        };
        match result {
            Ok(resp) => resp.into_json::<Value>().map_err(SeelError::Io),
            Err(ureq::Error::Status(status, resp)) => {
                let raw = resp.into_string().map_err(SeelError::Io)?;
                let body = match serde_json::from_str::<Value>(&raw) {
                    Ok(v) => v,
                    Err(_) => Value::String(raw),
                };
                Err(SeelError::Api(SeelApiError { status, body }))
            }
            Err(other) => Err(SeelError::Transport(Box::new(other))),
        }
    }

    // -- Merchants ----------------------------------------------------------

    /// Onboard one retailer. Call it when they enable the program;
    /// retailers group under your platform organization on Seel's side.
    /// Follow with create_orders_batch and at least 30 days of order
    /// history so Seel can run risk analysis.
    pub fn create_merchant(&self, payload: &Value) -> Result<Value, SeelError> {
        self.check("create_merchant", validate_merchant_payload(payload))?;
        self.request("POST", "/ecommerce/merchants", Some(payload))
    }

    /// Sync changed protection settings, or disable the program for a
    /// retailer - include the reason when disabling.
    pub fn update_merchant(&self, merchant_id: &str, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", &format!("/ecommerce/merchants/{}", path_param(merchant_id)), Some(payload))
    }

    // -- Quotes -------------------------------------------------------------

    /// Quote a cart at checkout. The response carries everything the
    /// storefront widget renders: price, display_amounts, widget_copy,
    /// extra_info. Re-quote whenever the cart changes - address, discount,
    /// item removed.
    ///
    /// The README's validation section and
    /// <https://developer.seel.com/reference/createquote> list the required
    /// fields, including
    /// price + sales_tax - allocated_discounts == final_price.
    pub fn create_quote(&self, payload: &Value) -> Result<Value, SeelError> {
        self.check("create_quote", validate_quote_payload(payload))?;
        self.request("POST", "/ecommerce/quotes", Some(payload))
    }

    pub fn get_quote(&self, quote_id: &str) -> Result<Value, SeelError> {
        self.request("GET", &format!("/ecommerce/quotes/{}", path_param(quote_id)), None)
    }

    // -- Orders -------------------------------------------------------------

    /// Sync every new order, opted in or not.
    ///
    /// On opt-in, `seel_services` must be an ARRAY of entries carrying
    /// `type`, `quote_id` and `price` from the latest quote - that mints the
    /// contract and fires `contract.created`. Sending `quote_id` at the top
    /// level instead returns 200 with `seel_services: null` and no error,
    /// which is why this checks the response as well as the request.
    ///
    /// Seel does not check the attach against the quote: a price that does
    /// not match the quoted premium, or line items that differ from the
    /// quoted cart, both still mint a contract. Keeping them consistent is
    /// the caller's job.
    pub fn create_order(&self, payload: &Value) -> Result<Value, SeelError> {
        self.check("create_order", validate_order_payload(payload))?;
        let response = self.request("POST", "/ecommerce/orders", Some(payload))?;
        let attached = payload
            .get("seel_services")
            .and_then(Value::as_array)
            .is_some_and(|s| !s.is_empty());
        if self.check_contract && attached {
            Self::check_contract_minted(payload, &response)?;
        }
        Ok(response)
    }

    /// Backfill order history at onboarding - at least 30 days.
    pub fn create_orders_batch(&self, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", "/ecommerce/orders/batch", Some(payload))
    }

    /// Sync order changes: line item removed, shipping address updated.
    pub fn update_order(&self, order_id: &str, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", &format!("/ecommerce/orders/{}", path_param(order_id)), Some(payload))
    }

    /// Cancel a synced order; its WFP coverage cancels with it.
    /// Refunding the WFP fee and tax to the shopper is the platform's job -
    /// see Cancellation in the README.
    pub fn cancel_order(&self, order_id: &str) -> Result<Value, SeelError> {
        self.request("POST", &format!("/ecommerce/orders/{}/cancel", path_param(order_id)), None)
    }

    // -- Fulfillments -------------------------------------------------------

    /// Send tracking number + carrier when the order ships.
    pub fn create_fulfillment(&self, order_id: &str, payload: &Value) -> Result<Value, SeelError> {
        self.request(
            "POST",
            &format!("/ecommerce/orders/{}/fulfillments", path_param(order_id)),
            Some(payload),
        )
    }

    /// Update tracking/delivery status after fulfillment.
    pub fn update_fulfillment(
        &self,
        order_id: &str,
        fulfillment_id: &str,
        payload: &Value,
    ) -> Result<Value, SeelError> {
        self.request(
            "POST",
            &format!(
                "/ecommerce/orders/{}/fulfillments/{}",
                path_param(order_id),
                path_param(fulfillment_id)
            ),
            Some(payload),
        )
    }

    // -- Claims -------------------------------------------------------------

    /// Register a claim when the shopper files in the platform's returns
    /// flow. Delivery-issue claims carry claim_type loss | damage | theft |
    /// delay plus claim_details with attachments; return-shipping claims
    /// carry claim_type return_shipping plus the RMA number, the return
    /// shipment (carrier, tracking, label cost), and the return addresses. Seel opens the claim as pending and fires
    /// the claim.created webhook.
    pub fn create_claim(&self, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", "/ecommerce/claims", Some(payload))
    }

    /// Submit the adjudication decision on programs where the platform
    /// adjudicates: decision accept | reject, with a reject_reason code and
    /// shopper-facing details on rejections. Claim items and amounts cannot
    /// be changed after creation. Seel records the outcome and fires
    /// claim.accepted or claim.rejected.
    pub fn update_claim(&self, claim_id: &str, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", &format!("/ecommerce/claims/{}", path_param(claim_id)), Some(payload))
    }

    pub fn get_claim(&self, claim_id: &str) -> Result<Value, SeelError> {
        self.request("GET", &format!("/ecommerce/claims/{}", path_param(claim_id)), None)
    }

    // -- Lookups (ad hoc; day-to-day state comes via webhooks) ---------------

    pub fn get_order(&self, order_id: &str) -> Result<Value, SeelError> {
        self.request("GET", &format!("/ecommerce/orders/{}", path_param(order_id)), None)
    }

    pub fn list_contracts(&self, query: &str) -> Result<Value, SeelError> {
        let path = if query.is_empty() {
            "/ecommerce/contracts".to_string()
        } else {
            format!("/ecommerce/contracts?{query}")
        };
        self.request("GET", &path, None)
    }

    pub fn list_claims(&self, query: &str) -> Result<Value, SeelError> {
        let path = if query.is_empty() {
            "/ecommerce/claims".to_string()
        } else {
            format!("/ecommerce/claims?{query}")
        };
        self.request("GET", &path, None)
    }
}

/// Verify Seel's X-Seel-Hmac-SHA256 header: Base64(HMAC-SHA256(body, secret)).
///
/// Webhook delivery is at-least-once: ACK with HTTP 200 within 10 seconds,
/// dedupe on the payload's id + type (retries reuse the same outer id).
///
/// The comparison is constant-time (the hmac crate's verify_slice); a
/// signature that fails to Base64-decode is invalid.
pub fn verify_webhook_signature(body: &[u8], signature_b64: &str, webhook_secret: &str) -> bool {
    let decoded = match BASE64_STANDARD.decode(signature_b64) {
        Ok(d) => d,
        Err(_) => return false,
    };
    // HMAC-SHA256 accepts keys of any size, so this cannot fail in practice.
    let mut mac = match Hmac::<Sha256>::new_from_slice(webhook_secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&decoded).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line_item() -> Value {
        json!({
            "line_item_id": "1", "product_id": "1", "product_title": "t", "quantity": 1,
            "price": 1.0, "allocated_discounts": 0.0, "sales_tax": 0.0, "final_price": 1.0,
            "currency": "USD", "requires_shipping": true, "image_urls": [],
            "category_1": "a", "category_2": "b", "is_final_sale": false,
            "shipping_origin": {"country": "US"}
        })
    }

    fn order() -> Value {
        json!({
            "merchant_id": "m", "order_id": "o", "order_number": "o", "created_ts": "1",
            "session_id": "s", "device_category": "desktop", "device_platform": "Web",
            "customer": {"customer_id": "c", "email": "e@x.com"},
            "shipping_address": {"address_1": "a", "city": "c", "state": "s",
                                 "zipcode": "z", "country": "US"},
            "line_items": [line_item()]
        })
    }

    #[test]
    fn complete_order_passes() {
        assert!(validate_order_payload(&order()).is_empty());
    }

    /// false and 0 are real values, not absences.
    #[test]
    fn false_and_zero_are_not_missing() {
        let mut o = order();
        o["line_items"][0]["requires_shipping"] = json!(false);
        o["line_items"][0]["allocated_discounts"] = json!(0);
        assert!(validate_order_payload(&o).is_empty());
    }

    /// The classic attach mistake: a top-level quote_id is silently ignored
    /// and the API still returns 200.
    #[test]
    fn top_level_quote_id_is_rejected() {
        let mut o = order();
        o["quote_id"] = json!("q1");
        let problems = validate_order_payload(&o);
        assert!(problems.iter().any(|p| p.contains("must go inside a seel_services entry")));
    }

    #[test]
    fn seel_services_object_is_rejected() {
        let mut o = order();
        o["seel_services"] = json!({"type": "x", "quote_id": "q", "price": 1.0});
        let problems = validate_order_payload(&o);
        assert!(problems.iter().any(|p| p.contains("must be an array")));
    }

    /// An order with no coverage is a normal sync, not an error. The
    /// absent case and the empty-array case must both pass.
    #[test]
    fn order_without_seel_services_passes() {
        let mut o = order();
        assert!(validate_order_payload(&o).is_empty(), "absent");
        o["seel_services"] = json!([]);
        assert!(validate_order_payload(&o).is_empty(), "empty array");
        o["seel_services"] = json!(null);
        assert!(validate_order_payload(&o).is_empty(), "null");
    }

    #[test]
    fn attached_order_requires_quote_id_and_price() {
        let mut o = order();
        o["seel_services"] = json!([{"type": "x"}]);
        let problems = validate_order_payload(&o);
        assert!(problems.iter().any(|p| p.contains("seel_services[0].quote_id")));
        assert!(problems.iter().any(|p| p.contains("seel_services[0].price")));
    }

    /// Omitting coverages returns a 500 from the API, so it must be caught
    /// locally. An empty array is accepted by the API and must pass here.
    #[test]
    fn merchant_requires_coverages_key_but_allows_empty() {
        let base = json!({
            "shop_id": "s", "admin_domain": "a", "shop_domain": "d",
            "shop_platform": "Shopify", "shop_currency": "USD", "shop_name": "n",
            "contact_name": "c", "contact_email": "e@x.com",
            "seel_services": [{"type": "t", "is_default_on": true}]
        });
        let problems = validate_merchant_payload(&base);
        assert!(problems.iter().any(|p| p.contains("seel_services[0].coverages")));

        let mut with_empty = base.clone();
        with_empty["seel_services"][0]["coverages"] = json!([]);
        assert!(validate_merchant_payload(&with_empty).is_empty());
    }

    /// Runs the shared validation cases against this port.
    ///
    /// The cases live in `server/validation-cases.json` and are read by the
    /// test suite in every language port. They exist to catch drift: the
    /// ports must make the same accept/reject decision and report the same
    /// field paths, and four hand-written suites would encode divergence
    /// rather than catch it.
    #[test]
    fn shared_validation_cases() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../validation-cases.json");
        let raw = std::fs::read_to_string(path).expect("read validation-cases.json");
        let doc: Value = serde_json::from_str(&raw).expect("parse validation-cases.json");
        let cases = doc["cases"].as_array().expect("cases array");
        assert!(!cases.is_empty(), "fixture is empty");

        for case in cases {
            let name = case["name"].as_str().unwrap_or("<unnamed>");
            let payload = &case["payload"];
            let problems = match case["operation"].as_str() {
                Some("quote") => validate_quote_payload(payload),
                Some("order") => validate_order_payload(payload),
                Some("merchant") => validate_merchant_payload(payload),
                other => panic!("{name}: unknown operation {other:?}"),
            };
            let joined = problems.join("; ");
            if case["expect_clean"].as_bool().unwrap_or(false) {
                assert!(problems.is_empty(), "{name}: expected no problems, got {joined}");
            } else {
                for fragment in case["expect_contains"].as_array().expect("expect_contains") {
                    let fragment = fragment.as_str().expect("fragment is a string");
                    assert!(joined.contains(fragment), "{name}: expected {fragment:?} in {joined:?}");
                }
            }
        }
    }

    /// The post-condition check on Create Order must agree across ports.
    #[test]
    fn shared_contract_cases() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../validation-cases.json");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

        for case in doc["contract_cases"].as_array().expect("contract_cases") {
            let name = case["name"].as_str().unwrap_or("<unnamed>");
            let minted =
                SeelClient::check_contract_minted(&case["request"], &case["response"]).is_ok();
            assert_eq!(
                minted,
                case["expect_minted"].as_bool().expect("expect_minted"),
                "{name}"
            );
        }

        for case in doc["uncovered_requests"].as_array().expect("uncovered_requests") {
            let name = case["name"].as_str().unwrap_or("<unnamed>");
            let checked = case["request"]["seel_services"]
                .as_array()
                .is_some_and(|s| !s.is_empty());
            assert_eq!(
                checked,
                case["expect_checked"].as_bool().expect("expect_checked"),
                "{name}"
            );
        }
    }

    #[test]
    fn path_param_encodes_separators() {
        assert_eq!(path_param("ORD1"), "ORD1");
        assert_eq!(path_param("x/cancel"), "x%2Fcancel");
        assert_eq!(path_param("a b"), "a%20b");
    }

    /// An id that escapes its segment must be refused, not folded into the
    /// upstream path.
    #[test]
    fn safe_path_param_refuses_escapes() {
        assert_eq!(safe_path_param("ORD1").as_deref(), Some("ORD1"));
        assert_eq!(safe_path_param("ORD1%2Fcancel"), None); // decoded slash
        assert_eq!(safe_path_param("a%00b"), None); // control character
        assert_eq!(safe_path_param("%zz"), None); // malformed escape
        assert_eq!(safe_path_param("%FF"), None); // invalid UTF-8
        assert_eq!(safe_path_param("batch"), None); // Seel's own batch endpoint
    }

    #[test]
    fn routes_match_seels_real_paths() {
        assert_eq!(parse_route("/v1/ecommerce/quotes"), Route::Quote);
        assert_eq!(parse_route("/v1/ecommerce/orders"), Route::CreateOrder);
        assert_eq!(
            parse_route("/v1/ecommerce/orders/ORD1"),
            Route::UpdateOrder("ORD1".to_string())
        );
        assert_eq!(
            parse_route("/v1/ecommerce/orders/ORD1/cancel"),
            Route::CancelOrder("ORD1".to_string())
        );
        assert_eq!(
            parse_route("/v1/ecommerce/orders/ORD1/fulfillments/F1"),
            Route::UpdateFulfillment("ORD1".to_string(), "F1".to_string())
        );
        assert_eq!(parse_route("/webhooks/seel"), Route::Webhook);
    }

    /// Cases where the ports could plausibly diverge. The other three
    /// match on regexes or split segments, and the shared fixture does not
    /// reach routing, so these are the local guard against that.
    #[test]
    fn route_edge_cases() {
        // A query string must not change routing.
        assert_eq!(parse_route("/v1/ecommerce/orders?trace=1"), Route::CreateOrder);
        // A trailing slash is not a create.
        assert_ne!(parse_route("/v1/ecommerce/orders/"), Route::CreateOrder);
        // Empty segments are not collapsed: this must not become an update
        // of an order called "cancel".
        assert_eq!(parse_route("/v1/ecommerce/orders//cancel"), Route::NotFound);
        assert_eq!(parse_route("//v1//ecommerce//orders"), Route::NotFound);
        // An escaping id is refused rather than routed.
        assert_eq!(parse_route("/v1/ecommerce/orders/ORD1%2Fcancel"), Route::BadPathParam);
        // The old prefix is gone.
        assert_eq!(parse_route("/api/seel/orders"), Route::NotFound);
    }

    #[test]
    fn quote_missing_nested_fields_reports_full_path() {
        let mut q = json!({
            "merchant_id": "m", "session_id": "s", "device_category": "desktop",
            "device_platform": "Web", "type": "t", "is_default_on": true,
            "customer": {"customer_id": "c", "email": "e@x.com"},
            "shipping_address": {"address_1": "a", "city": "c", "state": "s",
                                 "zipcode": "z", "country": "US"},
            "line_items": [line_item()]
        });
        assert!(validate_quote_payload(&q).is_empty());
        q["line_items"][0]["shipping_origin"] = json!({});
        let problems = validate_quote_payload(&q);
        assert!(problems.iter().any(|p| p.contains("line_items[0].shipping_origin.country")));
    }
}
