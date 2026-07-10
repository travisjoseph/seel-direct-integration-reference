//! Reference Rust client for Seel's ecommerce partner APIs (direct path).
//!
//! Wraps the public /v1/ecommerce/* APIs documented at
//! <https://developer.seel.com> - specifically the flow in the SaaS Platform
//! Integration Quickstart
//! (<https://developer.seel.com/docs/platform-direct-integration>):
//!
//! ```text
//! merchant enables program -> create_merchant (+ batch order-history backfill)
//! checkout                 -> create_quote (widget renders from the response)
//! order placed             -> create_order (ALL orders, opted-in or not;
//!                             seel_services carries quote_id + price on opt-in)
//! order shipped/delivered  -> create_fulfillment / update_fulfillment
//! order changed/cancelled  -> update_order / cancel_order
//! async status changes     -> webhooks (contract.*, claim.*), HMAC-signed
//! ```
//!
//! Blocking style with a handful of small crates, no async runtime -
//! intended to be readable enough to port to any stack.

use std::fmt;
use std::time::Duration;

use base64::prelude::*;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

pub const SANDBOX_BASE_URL: &str = "https://api-test.seel.com";
pub const PRODUCTION_BASE_URL: &str = "https://api.seel.com";
/// Single source of truth for the pinned API version.
pub const API_VERSION: &str = "2.6.0";

/// Returned on any non-2xx response. Carries the HTTP status and Seel's
/// JSON error body, which includes the actionable message (e.g. which
/// required field is missing) and a trace_id to quote in support requests.
#[derive(Debug)]
pub struct SeelApiError {
    pub status: u16,
    /// Seel's JSON error body, or a plain JSON string if the body was not
    /// valid JSON.
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
}

impl fmt::Display for SeelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SeelError::Api(err) => write!(f, "{err}"),
            SeelError::Transport(err) => write!(f, "transport error: {err}"),
            SeelError::Io(err) => write!(f, "response read error: {err}"),
        }
    }
}

impl std::error::Error for SeelError {}

pub struct SeelClient {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
    api_version: String,
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
        }
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

    /// Onboard one retailer. Called once per retailer when they enable
    /// the program (all grouped under your platform organization on Seel's
    /// side). Follow with create_orders_batch for at least 30 days of order
    /// history so Seel can run risk analysis.
    pub fn create_merchant(&self, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", "/ecommerce/merchants", Some(payload))
    }

    /// Toggle/disable the program for a retailer (include the reason
    /// when disabling), or sync changed protection settings.
    pub fn update_merchant(&self, merchant_id: &str, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", &format!("/ecommerce/merchants/{merchant_id}"), Some(payload))
    }

    // -- Quotes -------------------------------------------------------------

    /// Quote a cart at checkout. The response (price, display_amounts,
    /// widget_copy, extra_info) carries all the copy the storefront widget
    /// renders. Re-quote on cart changes: address change, discount applied,
    /// item removed.
    ///
    /// See the README "Required fields and validation rules" section and
    /// <https://developer.seel.com/reference/createquote> for the full
    /// required-field list, including the constraint
    /// price + sales_tax - allocated_discounts == final_price.
    pub fn create_quote(&self, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", "/ecommerce/quotes", Some(payload))
    }

    pub fn get_quote(&self, quote_id: &str) -> Result<Value, SeelError> {
        self.request("GET", &format!("/ecommerce/quotes/{quote_id}"), None)
    }

    // -- Orders -------------------------------------------------------------

    /// Sync every new order, whether or not the shopper opted in. When
    /// they did, include the seel_services object with the quote_id and
    /// price from the latest quote - this is what mints the contract and
    /// triggers the contract.created webhook. Line items must match the
    /// quoted cart.
    pub fn create_order(&self, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", "/ecommerce/orders", Some(payload))
    }

    /// Backfill historical orders (at least 30 days, at merchant
    /// onboarding).
    pub fn create_orders_batch(&self, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", "/ecommerce/orders/batch", Some(payload))
    }

    /// Sync order changes: line item removed, shipping address updated.
    pub fn update_order(&self, order_id: &str, payload: &Value) -> Result<Value, SeelError> {
        self.request("POST", &format!("/ecommerce/orders/{order_id}"), Some(payload))
    }

    /// Cancel a synced order; any WFP coverage on it cancels
    /// automatically. Refunding the WFP fee + tax to the shopper is the
    /// platform's job (see "Cancellation" in the README integration flow).
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
/// The comparison is constant-time (via the hmac crate's verify_slice); a
/// signature that fails to Base64-decode is simply invalid.
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
