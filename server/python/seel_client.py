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
  return or claim filed    -> create_claim (+ update_claim with the decision
                              when the platform adjudicates)
  async status changes     -> webhooks (contract.*, claim.*), HMAC-signed

Stdlib only. Written to be ported.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import urllib.error
import urllib.parse
import urllib.request

def _path_param(value: str) -> str:
    """Percent-encode one path segment.

    Ids arrive from callers and go straight into the upstream URL. Without
    this, an id containing "/" (or "%2F", which decodes to one) reaches a
    different endpoint than the method name implies - an update_order call
    with order_id "x/cancel" would cancel instead. safe="" so "/" is encoded
    too; the default would leave it.
    """
    return urllib.parse.quote(str(value), safe="")


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


class SeelValidationError(Exception):
    """Raised before any request when a payload is missing fields Seel
    requires. Carries the full list, so one call reports every problem
    instead of the API reporting them one 400 at a time.
    """

    def __init__(self, operation: str, problems: list[str]):
        self.operation = operation
        self.problems = problems
        super().__init__(
            f"{operation}: {'; '.join(problems)}. "
            "Pass validate=False to the client to skip these checks."
        )


class SeelContractNotMintedError(Exception):
    """Raised when create_order returns 200 but no contract was created.

    Seel reports a failed attach as contract_id: null on an otherwise
    successful response - there is no error status code. Without this
    check an integration looks healthy while covering nothing.
    """

    def __init__(self, response: dict, detail: str):
        self.response = response
        super().__init__(f"order created but no contract was minted: {detail}")


# Required-field sets, measured against sandbox on 2026-09-10 by removing one
# field per request from a known-good payload and recording the response.
#
# Treat these as a starting point, not a fixed contract. A newly provisioned
# account behaves this way; as an integration develops, Seel's implementation
# team works out which fields a merchant journey can actually supply and eases
# the validation accordingly, so an established account may accept less.
# Sending the full set is never wrong, but rejecting a payload locally could
# be, which is why validation is advisory and validate=False turns it off.
#
# A "[]" suffix means the rule applies to every element of that array.
_QUOTE_REQUIRED = {
    "": ["merchant_id", "session_id", "device_category", "device_platform", "type",
         "is_default_on", "customer", "shipping_address", "line_items"],
    "customer": ["customer_id", "email"],
    "shipping_address": ["address_1", "city", "state", "zipcode", "country"],
    "line_items[]": ["line_item_id", "product_id", "product_title", "quantity", "price",
                     "allocated_discounts", "sales_tax", "final_price", "currency",
                     "requires_shipping", "image_urls", "category_1", "category_2",
                     "is_final_sale", "shipping_origin"],
    "line_items[].shipping_origin": ["country"],
}

_ORDER_REQUIRED = {
    "": ["merchant_id", "order_id", "order_number", "created_ts", "session_id",
         "device_category", "device_platform", "customer", "shipping_address", "line_items"],
    "customer": ["customer_id", "email"],
    "shipping_address": ["address_1", "city", "state", "zipcode", "country"],
    "line_items[]": ["line_item_id", "product_id", "product_title", "quantity", "price",
                     "allocated_discounts", "sales_tax", "final_price", "currency",
                     "requires_shipping", "image_urls", "category_1", "category_2",
                     "is_final_sale", "shipping_origin"],
    "line_items[].shipping_origin": ["country"],
    # Only checked when seel_services is present - an order with no coverage
    # is a normal sync, not an error.
    "seel_services[]": ["type", "quote_id", "price"],
}

# coverages must be PRESENT but may be an empty list. Omitting it returns a
# 500 rather than a validation error, so catching it locally is the whole
# point of validating this call.
_MERCHANT_REQUIRED = {
    "": ["shop_id", "admin_domain", "shop_domain", "shop_platform", "shop_currency",
         "shop_name", "contact_name", "contact_email", "seel_services"],
    "seel_services[]": ["type", "coverages"],
}


# Shape expectations, checked alongside presence. A scalar where an object
# belongs is the archetypal payload mistake, and without this the nested
# rules silently skip it: _resolve_scope only descends into dicts, so
# {"customer": "nope"} would report no problems at all.
#
# Each entry is (parent scope, key, kind).
_QUOTE_SHAPES = [
    ("", "customer", "object"),
    ("", "shipping_address", "object"),
    ("", "line_items", "array_nonempty"),
    ("line_items[]", "shipping_origin", "object"),
]
_ORDER_SHAPES = _QUOTE_SHAPES + [("", "seel_services", "array")]
_MERCHANT_SHAPES = [("", "seel_services", "array_nonempty")]


# One vocabulary for type names across all four ports, so the same mistake
# reads the same way whichever one a partner runs.
_TYPE_NAMES = {dict: "object", list: "array", str: "string", bool: "boolean",
               int: "number", float: "number"}


def _type_name(value) -> str:
    return _TYPE_NAMES.get(type(value), type(value).__name__)


def _check_shapes(payload: dict, specs: list) -> list:
    problems = []
    for scope, key, kind in specs:
        for node, prefix in _resolve_scope(payload, scope):
            if key not in node or node[key] is None:
                continue  # absence is the required-field check's job
            value = node[key]
            path = f"{prefix}{key}"
            if kind == "object" and not isinstance(value, dict):
                problems.append(f"{path} must be an object, got {_type_name(value)}")
            elif kind.startswith("array"):
                if not isinstance(value, list):
                    problems.append(f"{path} must be an array, got {_type_name(value)}")
                elif kind == "array_nonempty" and not value:
                    problems.append(f"{path} must not be empty")
    return problems


def _is_absent(value) -> bool:
    """Is this value a non-answer? Key absence is handled by the caller.

    False and 0 are real values - is_default_on, requires_shipping and
    allocated_discounts all legitimately take them. An empty list is a real
    value too: merchant coverages: [] is accepted.
    """
    return value is None or value == ""


def _collect_missing(payload: dict, rules: dict) -> list[str]:
    missing = []
    for scope, fields in rules.items():
        for node, prefix in _resolve_scope(payload, scope):
            if node is None:
                continue
            for field in fields:
                if field not in node or _is_absent(node[field]):
                    missing.append(f"{prefix}{field}")
    return missing


def _resolve_scope(payload: dict, scope: str):
    """Yield (node, dotted_prefix) pairs a scope selects. Scopes are dotted
    paths where a "[]" suffix fans out over a list.
    """
    nodes = [(payload, "")]
    if scope:
        for part in scope.split("."):
            key, fan_out = (part[:-2], True) if part.endswith("[]") else (part, False)
            next_nodes = []
            for node, prefix in nodes:
                if not isinstance(node, dict):
                    continue
                child = node.get(key)
                if fan_out and isinstance(child, list):
                    next_nodes += [(item, f"{prefix}{key}[{i}].") for i, item in enumerate(child)
                                   if isinstance(item, dict)]
                elif not fan_out and isinstance(child, dict):
                    next_nodes.append((child, f"{prefix}{key}."))
            nodes = next_nodes
    return nodes


def validate_quote_payload(payload: dict) -> list[str]:
    """Return the problems with a Create Quote payload."""
    return ([f"missing required field {f}" for f in _collect_missing(payload, _QUOTE_REQUIRED)]
            + _check_shapes(payload, _QUOTE_SHAPES))


def validate_order_payload(payload: dict) -> list[str]:
    """Return the problems with a Create Order payload.

    seel_services is only checked for completeness when present: syncing an
    order the shopper did not opt into is normal. Two shape mistakes are
    checked separately, because the API accepts both and then fails in ways
    that do not look like failures.
    """
    rules = dict(_ORDER_REQUIRED)
    services = payload.get("seel_services")
    if not services or not isinstance(services, list):
        rules.pop("seel_services[]", None)
    problems = [f"missing required field {f}" for f in _collect_missing(payload, rules)]
    problems += _check_shapes(payload, _ORDER_SHAPES)

    # Create Order has no top-level quote_id. Sending one is the classic
    # attach mistake: the API returns 200 with seel_services: null and no
    # error, so the integration looks healthy while covering nothing.
    if "quote_id" in payload:
        problems.append(
            "quote_id must go inside a seel_services entry, not at the top level - "
            "a top-level quote_id is ignored and the order attaches no coverage"
        )
    return problems


def validate_merchant_payload(payload: dict) -> list[str]:
    """Return the problems with a Create Merchant payload."""
    return ([f"missing required field {f}" for f in _collect_missing(payload, _MERCHANT_REQUIRED)]
            + _check_shapes(payload, _MERCHANT_SHAPES))


class SeelClient:
    def __init__(self, api_key: str, base_url: str = SANDBOX_BASE_URL,
                 api_version: str = API_VERSION, timeout: int = 15,
                 validate: bool = True, check_contract: bool = True):
        self.api_key = api_key
        self.base_url = base_url.rstrip("/")
        self.api_version = api_version
        self.timeout = timeout
        # Pre-flight payload validation against the strict profile. Turn it
        # off for an account Seel has relaxed fields for, or to let the API
        # be the only authority on what a valid payload is.
        self.validate = validate
        # Post-condition check on create_order: did a contract actually
        # mint? Separate from validate on purpose. It reports a real failure
        # the API returns as a 200, not an opinion about required fields, so
        # turning validation off should not turn this off too.
        self.check_contract = check_contract

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

    def _check(self, operation: str, problems: list) -> None:
        if self.validate and problems:
            raise SeelValidationError(operation, problems)

    # -- Merchants ----------------------------------------------------------

    def create_merchant(self, payload: dict) -> dict:
        """Onboard one retailer. Call it when they enable the program;
        retailers group under your platform organization on Seel's side.
        Follow with create_orders_batch and at least 30 days of order
        history so Seel can run risk analysis.

        Each seel_services entry needs a coverages key. Omitting it returns
        a 500 rather than a validation error, so this checks it first."""
        self._check("create_merchant", validate_merchant_payload(payload))
        return self._request("POST", "/ecommerce/merchants", payload)

    def update_merchant(self, merchant_id: str, payload: dict) -> dict:
        """Sync changed protection settings, or disable the program for a
        retailer - include the reason when disabling."""
        return self._request("POST", f"/ecommerce/merchants/{_path_param(merchant_id)}", payload)

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
        self._check("create_quote", validate_quote_payload(payload))
        return self._request("POST", "/ecommerce/quotes", payload)

    def get_quote(self, quote_id: str) -> dict:
        return self._request("GET", f"/ecommerce/quotes/{_path_param(quote_id)}")

    # -- Orders -------------------------------------------------------------

    def create_order(self, payload: dict) -> dict:
        """Sync every new order, opted in or not.

        On opt-in, seel_services must be a LIST of entries carrying type,
        quote_id and price from the latest quote - that mints the contract
        and fires contract.created. Sending quote_id at the top level
        instead returns 200 with seel_services: null and no error, which is
        why this method checks the response as well as the request.

        Seel does not check the attach against the quote: a price that does
        not match the quoted premium, or line items that differ from the
        quoted cart, both still mint a contract. Keeping them consistent is
        the caller's job."""
        self._check("create_order", validate_order_payload(payload))
        response = self._request("POST", "/ecommerce/orders", payload)
        if self.check_contract and payload.get("seel_services"):
            self._check_contract_minted(payload, response)
        return response

    @staticmethod
    def _check_contract_minted(payload: dict, response: dict) -> None:
        """Fail loudly when an attach silently did not take.

        Every failed attach observed so far is contract_id: null on a 200
        rather than a status code, so nothing else in the stack notices.
        """
        services = response.get("seel_services")
        if not services:
            raise SeelContractNotMintedError(
                response,
                f"sent {len(payload['seel_services'])} seel_services entr"
                f"{'y' if len(payload['seel_services']) == 1 else 'ies'}, "
                f"response seel_services is {services!r}. Check seel_services is a "
                "list and quote_id is inside it, not at the top level.",
            )
        for entry in services:
            if not isinstance(entry, dict) or entry.get("contract_id"):
                continue
            raise SeelContractNotMintedError(
                response,
                f"service {entry.get('type')!r} returned contract_id=None "
                f"(status={entry.get('status')!r}, error={entry.get('error')!r})",
            )

    def create_orders_batch(self, payload: dict) -> dict:
        """Backfill order history at onboarding - at least 30 days."""
        return self._request("POST", "/ecommerce/orders/batch", payload)

    def update_order(self, order_id: str, payload: dict) -> dict:
        """Sync order changes: line item removed, shipping address updated."""
        return self._request("POST", f"/ecommerce/orders/{_path_param(order_id)}", payload)

    def cancel_order(self, order_id: str) -> dict:
        """Cancel a synced order; its WFP coverage cancels with it.
        Refunding the WFP fee and tax to the shopper is the platform's job -
        see Cancellation in the README."""
        return self._request("POST", f"/ecommerce/orders/{_path_param(order_id)}/cancel")

    # -- Fulfillments -------------------------------------------------------

    def create_fulfillment(self, order_id: str, payload: dict) -> dict:
        """Send tracking number + carrier when the order ships."""
        return self._request("POST", f"/ecommerce/orders/{_path_param(order_id)}/fulfillments", payload)

    def update_fulfillment(self, order_id: str, fulfillment_id: str, payload: dict) -> dict:
        """Update tracking/delivery status after fulfillment."""
        return self._request(
            "POST",
            f"/ecommerce/orders/{_path_param(order_id)}/fulfillments/{_path_param(fulfillment_id)}",
            payload
        )

    # -- Claims -------------------------------------------------------------

    def create_claim(self, payload: dict) -> dict:
        """Register a claim when the shopper files in the platform's returns
        flow. Delivery-issue claims carry claim_type loss | damage | theft |
        delay plus claim_details with attachments; return-shipping claims
        carry claim_type return_shipping plus the RMA number, the return
        shipment (carrier, tracking, label cost), and the return addresses. Seel opens the claim as pending and fires
        the claim.created webhook."""
        return self._request("POST", "/ecommerce/claims", payload)

    def update_claim(self, claim_id: str, payload: dict) -> dict:
        """Submit the adjudication decision on programs where the platform
        adjudicates: decision accept | reject, with a reject_reason code and
        shopper-facing details on rejections. Claim items and amounts cannot
        be changed after creation. Seel records the outcome and fires
        claim.accepted or claim.rejected."""
        return self._request("POST", f"/ecommerce/claims/{_path_param(claim_id)}", payload)

    def get_claim(self, claim_id: str) -> dict:
        return self._request("GET", f"/ecommerce/claims/{_path_param(claim_id)}")

    # -- Lookups (ad hoc; day-to-day state comes via webhooks) ---------------

    def get_order(self, order_id: str) -> dict:
        return self._request("GET", f"/ecommerce/orders/{_path_param(order_id)}")

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
