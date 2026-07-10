"""Python client for Seel's ecommerce APIs (the direct-integration path).

Wraps the public /v1/ecommerce/* APIs and follows the flow in the SaaS
Platform Integration Quickstart
(https://developer.seel.com/docs/platform-direct-integration):

  merchant enables program -> create_merchant (+ batch order-history backfill)
  checkout                 -> create_quote (widget renders from the response)
  order placed             -> create_order (all orders, opted in or not;
                              seel_services carries quote_id + price on opt-in)
  order shipped/delivered  -> create_fulfillment / update_fulfillment
  order changed/cancelled  -> update_order / cancel_order
  async status changes     -> webhooks (contract.*, claim.*), HMAC-signed

Stdlib only. Written to be ported.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import urllib.error
import urllib.request

SANDBOX_BASE_URL = "https://api-test.seel.com"
PRODUCTION_BASE_URL = "https://api.seel.com"
API_VERSION = "2.6.0"  # the pinned API version; all four language ports match


class SeelAPIError(Exception):
    """Raised on any non-2xx response. Carries the status and Seel's JSON
    error body: the message names the offending field, and the trace_id is
    what Seel support will ask for.
    """

    def __init__(self, status: int, body: dict | str):
        self.status = status
        self.body = body
        message = body.get("error", str(body)) if isinstance(body, dict) else str(body)
        super().__init__(f"Seel API {status}: {message}")


class SeelClient:
    def __init__(self, api_key: str, base_url: str = SANDBOX_BASE_URL,
                 api_version: str = API_VERSION, timeout: int = 15):
        self.api_key = api_key
        self.base_url = base_url.rstrip("/")
        self.api_version = api_version
        self.timeout = timeout

    def _request(self, method: str, path: str, payload: dict | None = None) -> dict:
        # Header names are case-insensitive per RFC 9110; urllib normalizes
        # their casing on the wire and Seel accepts any casing.
        req = urllib.request.Request(
            self.base_url + "/v1" + path,
            data=json.dumps(payload).encode() if payload is not None else None,
            headers={
                "X-Seel-Api-Key": self.api_key,
                "X-Seel-Api-Version": self.api_version,
                "Content-Type": "application/json",
            },
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return json.loads(resp.read().decode())
        except urllib.error.HTTPError as exc:
            raw = exc.read().decode(errors="replace")
            try:
                body = json.loads(raw)
            except json.JSONDecodeError:
                body = raw
            raise SeelAPIError(exc.code, body) from exc

    # -- Merchants ----------------------------------------------------------

    def create_merchant(self, payload: dict) -> dict:
        """Onboard one retailer. Call it when they enable the program;
        retailers group under your platform organization on Seel's side.
        Follow with create_orders_batch and at least 30 days of order
        history so Seel can run risk analysis."""
        return self._request("POST", "/ecommerce/merchants", payload)

    def update_merchant(self, merchant_id: str, payload: dict) -> dict:
        """Sync changed protection settings, or disable the program for a
        retailer - include the reason when disabling."""
        return self._request("POST", f"/ecommerce/merchants/{merchant_id}", payload)

    # -- Quotes -------------------------------------------------------------

    def create_quote(self, payload: dict) -> dict:
        """Quote a cart at checkout. The response carries everything the
        storefront widget renders: price, display_amounts, widget_copy,
        extra_info. Re-quote whenever the cart changes - address, discount,
        item removed.

        The README's validation section and
        https://developer.seel.com/reference/createquote list the required
        fields, including
        price + sales_tax - allocated_discounts == final_price."""
        return self._request("POST", "/ecommerce/quotes", payload)

    def get_quote(self, quote_id: str) -> dict:
        return self._request("GET", f"/ecommerce/quotes/{quote_id}")

    # -- Orders -------------------------------------------------------------

    def create_order(self, payload: dict) -> dict:
        """Sync every new order, opted in or not. On opt-in, include the
        seel_services object with the quote_id and price from the latest
        quote - that mints the contract and fires the contract.created
        webhook. Line items must match the quoted cart."""
        return self._request("POST", "/ecommerce/orders", payload)

    def create_orders_batch(self, payload: dict) -> dict:
        """Backfill order history at onboarding - at least 30 days."""
        return self._request("POST", "/ecommerce/orders/batch", payload)

    def update_order(self, order_id: str, payload: dict) -> dict:
        """Sync order changes: line item removed, shipping address updated."""
        return self._request("POST", f"/ecommerce/orders/{order_id}", payload)

    def cancel_order(self, order_id: str) -> dict:
        """Cancel a synced order; its WFP coverage cancels with it.
        Refunding the WFP fee and tax to the shopper is the platform's job -
        see Cancellation in the README."""
        return self._request("POST", f"/ecommerce/orders/{order_id}/cancel")

    # -- Fulfillments -------------------------------------------------------

    def create_fulfillment(self, order_id: str, payload: dict) -> dict:
        """Send tracking number + carrier when the order ships."""
        return self._request("POST", f"/ecommerce/orders/{order_id}/fulfillments", payload)

    def update_fulfillment(self, order_id: str, fulfillment_id: str, payload: dict) -> dict:
        """Update tracking/delivery status after fulfillment."""
        return self._request(
            "POST", f"/ecommerce/orders/{order_id}/fulfillments/{fulfillment_id}", payload
        )

    # -- Lookups (ad hoc; day-to-day state comes via webhooks) ---------------

    def get_order(self, order_id: str) -> dict:
        return self._request("GET", f"/ecommerce/orders/{order_id}")

    def list_contracts(self, query: str = "") -> dict:
        return self._request("GET", "/ecommerce/contracts" + (f"?{query}" if query else ""))

    def list_claims(self, query: str = "") -> dict:
        return self._request("GET", "/ecommerce/claims" + (f"?{query}" if query else ""))


def verify_webhook_signature(body: bytes, signature_b64: str, webhook_secret: str) -> bool:
    """Verify Seel's X-Seel-Hmac-SHA256 header: Base64(HMAC-SHA256(body, secret)).

    Webhook delivery is at-least-once: ACK with HTTP 200 within 10 seconds,
    dedupe on the payload's id + type (retries reuse the same outer id).
    """
    expected = base64.b64encode(
        hmac.new(webhook_secret.encode(), body, hashlib.sha256).digest()
    ).decode()
    return hmac.compare_digest(expected, signature_b64)
