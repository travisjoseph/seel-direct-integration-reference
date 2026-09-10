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
    /// `create_order` returned 200 but no contract was created. Seel reports
    /// a failed attach as `contract_id: null` on an otherwise successful
    /// response, so without this check an integration looks healthy while
    /// covering nothing.
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
                "{operation}: {}. Build the client with validation disabled to skip these checks.",
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
// Requiredness is PER-ACCOUNT. Seel validates a strict default profile and
// relaxes individual fields for some accounts, so an account may
// legitimately accept less than this. These sets are the strict profile:
// sending them is never wrong, but rejecting a payload locally could be.
// That is why validation is advisory and can be turned off.
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

fn as_problems(missing: Vec<String>) -> Vec<String> {
    missing
        .into_iter()
        .map(|f| format!("missing required field {f}"))
        .collect()
}

/// Return the problems with a Create Quote payload.
pub fn validate_quote_payload(payload: &Value) -> Vec<String> {
    as_problems(collect_missing(payload, QUOTE_REQUIRED))
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
    if let Some(value) = services {
        if !value.is_null() && !value.is_array() {
            problems.push(
                "seel_services must be an array - an object is rejected by the parser \
                 with a 500"
                    .to_string(),
            );
        }
    }
    problems
}

/// Return the problems with a Create Merchant payload.
pub fn validate_merchant_payload(payload: &Value) -> Vec<String> {
    as_problems(collect_missing(payload, MERCHANT_REQUIRED))
}

pub struct SeelClient {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
    api_version: String,
    /// Pre-flight validation. Turn off for an account Seel has relaxed
    /// fields for, or to let the API be the only authority.
    validate: bool,
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
        }
    }

    /// Turn pre-flight validation off. Do this for an account Seel has
    /// relaxed required fields for, or to let the API be the only
    /// authority on what a valid payload is.
    #[must_use]
    pub fn without_validation(mut self) -> Self {
        self.validate = false;
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
            let minted = entry
                .get("contract_id")
                .is_some_and(|c| !c.is_null() && c.as_str() != Some(""));
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
        self.request("POST", &format!("/ecommerce/merchants/{merchant_id}"), Some(payload))
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
        self.request("GET", &format!("/ecommerce/quotes/{quote_id}"), None)
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
        if self.validate && attached {
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
        self.request("POST", &format!("/ecommerce/orders/{order_id}"), Some(payload))
    }

    /// Cancel a synced order; its WFP coverage cancels with it.
    /// Refunding the WFP fee and tax to the shopper is the platform's job -
    /// see Cancellation in the README.
    pub fn cancel_order(&self, order_id: &str) -> Result<Value, SeelError> {
        self.request("POST", &format!("/ecommerce/orders/{order_id}/cancel"), None)
    }

    // -- Fulfillments -------------------------------------------------------

    /// Send tracking number + carrier when the order ships.
    pub fn create_fulfillment(&self, order_id: &str, payload: &Value) -> Result<Value, SeelError> {
        self.request(
            "POST",
            &format!("/ecommerce/orders/{order_id}/fulfillments"),
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
            &format!("/ecommerce/orders/{order_id}/fulfillments/{fulfillment_id}"),
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
        self.request("POST", &format!("/ecommerce/claims/{claim_id}"), Some(payload))
    }

    pub fn get_claim(&self, claim_id: &str) -> Result<Value, SeelError> {
        self.request("GET", &format!("/ecommerce/claims/{claim_id}"), None)
    }

    // -- Lookups (ad hoc; day-to-day state comes via webhooks) ---------------

    pub fn get_order(&self, order_id: &str) -> Result<Value, SeelError> {
        self.request("GET", &format!("/ecommerce/orders/{order_id}"), None)
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

    /// An order with no coverage is a normal sync, not an error.
    #[test]
    fn order_without_seel_services_passes() {
        assert!(validate_order_payload(&order()).is_empty());
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
