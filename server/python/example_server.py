"""Example backend for the direct-integration path. Stdlib only.

Routes:
  POST /v1/ecommerce/quotes  - browser quote proxy: attaches the server-side API
                          key and forwards to Seel's Quote API (the widget
                          never sees the key)

  POST /v1/ecommerce/orders                                     - create order
  POST /v1/ecommerce/orders/{order_id}                          - update order
  POST /v1/ecommerce/orders/{order_id}/cancel                   - cancel order
  POST /v1/ecommerce/orders/{order_id}/fulfillments             - create fulfillment
  POST /v1/ecommerce/orders/{order_id}/fulfillments/{fid}       - update fulfillment

  POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
                          events: verifies HMAC, ACKs 200 fast, then hands
                          off for internal fan-out

The order and fulfillment routes mirror Seel's own path shape, so a caller
already written against Seel's API moves over by changing the base URL and
nothing else.

Two deployments use these routes differently:

  Single retailer - the retailer's own backend holds the API key and calls
  Seel directly. The order and fulfillment routes are optional here; call
  SeelClient from your order pipeline instead if that fits better.

  Partner platform proxy - the partner platform holds one API key for every retailer on it,
  retailers point at the partner platform instead of at Seel, and the partner platform
  resolves which merchant each request belongs to. Retailers hold no Seel
  credentials at all. See authenticate_caller() and resolve_merchant_id()
  below, which are where a partner platform starts, and SEEL_RETAILER_MERCHANTS,
  which binds each retailer storefront's origin to its merchant. They are
  not the whole job: order_id comes off the URL and is never checked
  against the caller, so nothing here stops one retailer touching
  another's order. That mapping belongs to the partner platform - see the README.

Run:
  SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... python3 example_server.py

To drive the widget demo against a live sandbox, two steps - this server
doesn't serve the demo page:
  1. run this server
  2. in widget/demo.html, replace the mock quoteFetcher with
     configure({ quoteEndpoint: "http://localhost:8787/v1/ecommerce/quotes" })
"""

import json
import os
import re
import socket
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Optional
from urllib.parse import unquote

from seel_client import (
    SANDBOX_BASE_URL,
    SeelAPIError,
    SeelClient,
    SeelContractNotMintedError,
    SeelTimeoutError,
    SeelValidationError,
    verify_webhook_signature,
)

# Loopback by default: this demo authenticates nobody, so it must not be
# reachable from the network unless someone chooses that.
HOST = os.environ.get("HOST", "127.0.0.1")
PORT = int(os.environ.get("PORT", "8787"))
# Request bodies larger than this get a 413 before any parsing. A quote or
# order is a few KB; the cap exists so a single caller cannot make the
# process buffer arbitrary input.
MAX_BODY_BYTES = 1048576
# A body past the cap is read and dropped before the 413 goes out, so the
# connection does not close on unread bytes: that close resets the
# connection, and a client still sending sees the reset before it reads
# the 413 (RFC 9112 section 9.6). Nothing is kept; this bounds how long a
# client that stalls mid-body holds the handler.
DRAIN_TIMEOUT_SECONDS = 5
API_KEY = os.environ.get("SEEL_API_KEY", "")
WEBHOOK_SECRET = os.environ.get("SEEL_WEBHOOK_SECRET", "")
BASE_URL = os.environ.get("SEEL_BASE_URL", SANDBOX_BASE_URL)
# Program values from Seel onboarding. When set, the proxy stamps them into
# every quote request, so storefront code stays identical across programs.
MERCHANT_ID = os.environ.get("SEEL_MERCHANT_ID", "")
QUOTE_TYPE = os.environ.get("SEEL_QUOTE_TYPE", "")


def normalize_origin(origin: str) -> str:
    return origin.strip().lower().rstrip("/")


def parse_retailer_merchants(raw: str) -> dict:
    """Parse SEEL_RETAILER_MERCHANTS into {storefront origin: merchant_id}.

    Raises ValueError on anything malformed. A bad table must stop startup:
    running on with a partial or empty one would refuse every retailer, or
    worse, bind a storefront to the wrong merchant.
    """
    try:
        table = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ValueError(f"SEEL_RETAILER_MERCHANTS is not valid JSON: {exc}") from None
    if not isinstance(table, dict):
        raise ValueError("SEEL_RETAILER_MERCHANTS must be a JSON object of origin to merchant_id")
    parsed = {}
    for origin, merchant_id in table.items():
        if not isinstance(merchant_id, str) or not merchant_id:
            raise ValueError(f"SEEL_RETAILER_MERCHANTS[{origin!r}] must be a non-empty string")
        key = normalize_origin(origin)
        if parsed.get(key, merchant_id) != merchant_id:
            raise ValueError(f"SEEL_RETAILER_MERCHANTS maps {key!r} to two merchants")
        parsed[key] = merchant_id
    return parsed


_raw_table = os.environ.get("SEEL_RETAILER_MERCHANTS", "")
RETAILER_MERCHANTS = parse_retailer_merchants(_raw_table) if _raw_table else None
# Both set is a contradiction, not a precedence question: SEEL_MERCHANT_ID
# would silently stamp one merchant onto every retailer in the table.
if MERCHANT_ID and RETAILER_MERCHANTS is not None:
    raise ValueError("set SEEL_MERCHANT_ID or SEEL_RETAILER_MERCHANTS, not both")

client = SeelClient(api_key=API_KEY, base_url=BASE_URL)

# Routes mirror Seel's real paths, prefix included, so a caller already
# written against Seel moves onto a partner platform by changing the base URL and
# nothing else. The clients build "<base>/v1/ecommerce/...", so anything
# shorter would 404 every call.
QUOTE_PATH = "/v1/ecommerce/quotes"
WEBHOOK_PATH = "/webhooks/seel"
ORDERS_PATH = re.compile(r"^/v1/ecommerce/orders$")
ORDER_PATH = re.compile(r"^/v1/ecommerce/orders/([^/]+)$")
ORDER_CANCEL_PATH = re.compile(r"^/v1/ecommerce/orders/([^/]+)/cancel$")
FULFILLMENTS_PATH = re.compile(r"^/v1/ecommerce/orders/([^/]+)/fulfillments$")
FULFILLMENT_PATH = re.compile(r"^/v1/ecommerce/orders/([^/]+)/fulfillments/([^/]+)$")


# Segments that are Seel endpoints in their own right and so can never be
# an order id. Seel's own collection endpoints live alongside order ids, so an id
# that equals one of them would reach the collection instead. "batch" is
# POST /v1/ecommerce/orders/batch, the order-history backfill: routed as an
# order id it would proxy an unstamped, unvalidated batch write.
RESERVED_PATH_SEGMENTS = {"batch"}


def safe_path_param(raw: str):
    """Percent-decode one path segment, or return None if it escapes.

    A segment arrives encoded and is interpolated into the upstream URL, so
    a decoded "/" would reach a different endpoint than the route implies:
    "orders/ORD1%2Fcancel" matches the update-order route and would perform
    a cancel. Control characters are refused for the same reason. With one
    API key shared across retailers this is a privilege boundary, not a
    cosmetic check.
    """
    # unquote leaves a malformed escape as literal text rather than
    # raising, so check the escapes explicitly. Without this the segment is
    # forwarded upstream re-encoded, where Node and Java reject it.
    i = 0
    while i < len(raw):
        if raw[i] == "%":
            if len(raw) < i + 3 or any(c not in "0123456789abcdefABCDEF" for c in raw[i + 1 : i + 3]):
                return None
            i += 3
        else:
            i += 1
    # errors="strict" so invalid UTF-8 is refused rather than replaced with
    # U+FFFD and forwarded, which Node and Rust reject.
    try:
        decoded = unquote(raw, errors="strict")
    except UnicodeDecodeError:
        return None
    if "/" in decoded or any(ord(c) < 0x20 or ord(c) == 0x7F for c in decoded):
        return None
    if decoded in RESERVED_PATH_SEGMENTS:
        return None
    # A segment that is only dots is refused too: a decoded ".." is not a
    # slash, but HTTP clients normalize it away, so "orders/%2E%2E/cancel"
    # leaves this proxy as a request to /v1/ecommerce/cancel - a different
    # endpoint than the route names.
    if decoded.strip(".") == "":
        return None
    return decoded


@dataclass(frozen=True)
class Caller:
    """Who is calling. origin is the normalized Origin header, or None."""

    origin: Optional[str]


class MerchantBindingError(Exception):
    """The request cannot be bound to a merchant; the handler answers 403."""


def authenticate_caller(headers) -> Optional[Caller]:
    """Identify the caller, or return None to answer 401.

    Browser quote requests come from a retailer's storefront, and the
    browser sets Origin itself; page script cannot change it. That makes
    the origin the caller's identity for the quote route, and
    resolve_merchant_id() looks it up in SEEL_RETAILER_MERCHANTS. A
    non-browser client can send any Origin it likes, so this binds what a
    shopper's browser can do, not what a script with the proxy URL can.

    Server-to-server routes (orders, fulfillments, cancel) have no Origin.
    This demo accepts them from anyone, which is only safe because it holds
    a sandbox key and listens on localhost. A partner platform authenticates its
    retailers' backends with its own credentials, returns the retailer
    identity here, and maps it to a merchant the same way the origin is
    mapped. Retailers never receive a Seel API key: one key covers every
    retailer on the partner platform and would let any holder act as any other.
    """
    origin = headers.get("Origin")
    return Caller(origin=normalize_origin(origin) if origin else None)


def resolve_merchant_id(caller: Caller, params: dict) -> str:
    """Return the merchant ID to stamp, or raise MerchantBindingError.

    Single retailer: SEEL_MERCHANT_ID is set once in the environment and
    stamped onto everything, so storefront and pipeline code carry no
    program-specific values.

    Partner platform proxy: SEEL_RETAILER_MERCHANTS maps each storefront origin to
    its merchant. The merchant comes from the caller, never the body, which
    is what stops one retailer quoting or ordering against another's
    merchant ID. A body merchant_id that disagrees is refused rather than
    overwritten, so a misconfigured storefront fails loudly. The origin is
    the only identity this demo knows, so with a table set a backend call
    without one, such as create order, is refused until the partner platform maps
    its own retailer credentials here too.

    With neither set, the body value is forwarded so the unconfigured demo
    still runs. Any caller can then name any merchant.
    """
    if MERCHANT_ID:
        return MERCHANT_ID
    if RETAILER_MERCHANTS is None:
        return params.get("merchant_id", "")
    merchant_id = RETAILER_MERCHANTS.get(caller.origin)
    if merchant_id is None:
        raise MerchantBindingError("request origin is not a registered storefront")
    if "merchant_id" in params and params["merchant_id"] != merchant_id:
        raise MerchantBindingError(
            "merchant_id in the request body does not match the merchant for this storefront"
        )
    return merchant_id


def allowed_origin(headers) -> Optional[str]:
    """The Access-Control-Allow-Origin value for this request, or None.

    With a retailer table, only a registered storefront may read responses,
    so the header echoes that origin and is omitted for any other.
    """
    if RETAILER_MERCHANTS is None:
        return "*"
    origin = normalize_origin(headers.get("Origin") or "")
    return origin if origin in RETAILER_MERCHANTS else None


def handle_webhook_event(event: dict) -> None:
    """Route a delivered webhook into your own systems.

    Before any of this fires, the endpoint has to be registered with Seel.
    Seel has no self-serve way to register this URL. There is no webhook field on
    Create or Update Merchant and no registration endpoint - ask your Seel contact
    to configure it, and tell them which events you want. Do it once per
    environment: a sandbox registration does not carry over to production.

    Internal fan-out. Map merchant_id/order_id to your own retailer code
    here and route to your systems. Dedupe on id + type first, since
    delivery is at-least-once. In production, queue this work off the
    request thread instead of processing inline."""
    print(f"[webhook] {event.get('type')} id={event.get('id')}")


class Handler(BaseHTTPRequestHandler):
    def _respond(self, status: int, body: dict) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self._send_cors_origin()
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _send_cors_origin(self) -> None:
        origin = allowed_origin(self.headers)
        if origin:
            self.send_header("Access-Control-Allow-Origin", origin)
        if RETAILER_MERCHANTS is not None:
            self.send_header("Vary", "Origin")

    def do_OPTIONS(self):  # CORS preflight for storefront pages
        self.send_response(204)
        self._send_cors_origin()
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Allow-Methods", "POST, OPTIONS")
        self.end_headers()

    def _read_body(self):
        """Read the request body, or answer 413/400 and return None.

        Content-Length is checked before reading and a chunked body is
        capped as it streams, so neither framing can push more than
        MAX_BODY_BYTES into memory.
        """
        try:
            if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
                body = self._read_chunked()
            else:
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                except ValueError:
                    length = 0
                if length > MAX_BODY_BYTES:
                    self._discard(length)
                    body = None
                else:
                    body = self.rfile.read(max(length, 0))
        except ValueError:
            self._respond(400, {"error": "malformed chunked request body"})
            return None
        except socket.timeout:
            # Only set while dropping a body past the cap, so the 413 still
            # fits; the close after it may reset, as it would have anyway.
            body = None
        if body is None:
            self._respond(413, {"error": f"request body exceeds {MAX_BODY_BYTES} bytes"})
        return body

    def _discard(self, size: int) -> None:
        """Read and drop size bytes of body in slices, never holding them."""
        self.connection.settimeout(DRAIN_TIMEOUT_SECONDS)
        while size > 0:
            read = self.rfile.read(min(size, 65536))
            if not read:
                return
            size -= len(read)

    def _read_chunked(self):
        """Decode a Transfer-Encoding: chunked body (RFC 9112 section 7.1).

        http.server does not decode it, so a signed webhook sent chunked
        would otherwise verify against an empty body and 401. Returns None
        once the declared chunks exceed the cap, after decoding and
        dropping the rest; raises ValueError on malformed framing.
        """
        body = bytearray()
        while True:
            # int(x, 16) alone also takes "0x2", "+2" and "0_2", which other
            # HTTP parsers reject; plain hex digits only.
            digits = self.rfile.readline(65537).split(b";", 1)[0].strip()
            if not digits or digits.strip(b"0123456789abcdefABCDEF"):
                raise ValueError("bad chunk size")
            size = int(digits, 16)
            if size == 0:
                break
            if body is not None and len(body) + size > MAX_BODY_BYTES:
                body = None
            if body is None:
                self._discard(size)
            else:
                chunk = self.rfile.read(size)
                if len(chunk) != size:
                    raise ValueError("truncated chunk")
                body += chunk
            if self.rfile.read(2) != b"\r\n":
                raise ValueError("truncated chunk")
        while self.rfile.readline(65537) not in (b"\r\n", b"\n", b""):
            pass  # trailer fields carry nothing this server uses
        return None if body is None else bytes(body)

    def _stamp_merchant(self, caller: Caller, params: dict) -> bool:
        """Stamp the resolved merchant into params, or answer 403 and return False."""
        try:
            merchant_id = resolve_merchant_id(caller, params)
        except MerchantBindingError as exc:
            self._respond(403, {"error": str(exc)})
            return False
        if merchant_id:
            params["merchant_id"] = merchant_id
        return True

    def _forward(self, label: str, call) -> None:
        """Call Seel and mirror the result back to the caller.

        Each failure has to stay distinguishable. Collapsing them into one
        502 would report a payload that never left this process as an
        upstream outage, and would invite a retry on an order Seel has
        already accepted.
        """
        try:
            self._respond(200, call())
        except SeelValidationError as exc:
            # The request never left this process, so this is the caller's
            # bug. Hand back every problem at once.
            self._respond(400, {"error": str(exc), "problems": exc.problems})
        except SeelContractNotMintedError as exc:
            # Seel accepted the order and minted no contract. 502 would be
            # wrong twice over: the upstream call succeeded, and a retry
            # would duplicate the order.
            # flush: stdout is block-buffered when it is not a tty, and
            # this process never exits, so an unflushed line is never seen.
            print(f"[proxy] {label}: {exc}", flush=True)
            self._respond(409, {"error": str(exc), "seel_response": exc.response})
        except SeelAPIError as exc:
            # Forward Seel's status and error body - it names the
            # offending field.
            self._respond(exc.status, exc.body if isinstance(exc.body, dict) else {"error": str(exc)})
        except SeelTimeoutError:
            # Seel may have processed the request before we gave up, so
            # this must not read as "safe to retry" the way a 502 does.
            print(f"[proxy] {label}: timed out waiting for Seel", flush=True)
            self._respond(504, {"error": "timed out waiting for Seel; the request may have "
                                         "been processed, check before retrying"})
        except Exception as exc:
            # Log before answering: a bare 502 leaves the operator unable to
            # tell a connection failure from a bug in this handler.
            print(f"[proxy] {label} failed: {exc!r}", flush=True)
            self._respond(502, {"error": f"upstream {label} request failed"})

    def _json_body(self, body: bytes):
        """Parse a JSON object body, or answer 400 and return None."""
        try:
            params = json.loads(body)
        except (ValueError, RecursionError):
            # ValueError covers JSONDecodeError and UnicodeDecodeError;
            # RecursionError is neither, and a deeply nested body would
            # otherwise escape do_POST and close the socket with no
            # response at all.
            self._respond(400, {"error": "request body must be JSON"})
            return None
        if not isinstance(params, dict):
            self._respond(400, {"error": "request body must be a JSON object"})
            return None
        return params

    def do_GET(self):
        # Only POST routes exist; answer in JSON like the other ports rather
        # than letting BaseHTTPRequestHandler emit its HTML 501 page.
        self._respond(404, {"error": "not found"})

    # Every other verb answers the same way, for the same reason.
    do_PUT = do_PATCH = do_DELETE = do_HEAD = do_GET

    def do_POST(self):
        body = self._read_body()
        if body is None:
            return
        # Match on the path only. Ports that keep the query string here
        # would 404 a request the others route.
        path = self.path.split("?", 1)[0]

        caller = None
        if path != WEBHOOK_PATH:
            caller = authenticate_caller(self.headers)
            if caller is None:
                self._respond(401, {"error": "unauthorized"})
                return

        if path == QUOTE_PATH:
            params = self._json_body(body)
            if params is None:
                return
            if not self._stamp_merchant(caller, params):
                return
            if QUOTE_TYPE:
                params["type"] = QUOTE_TYPE
            self._forward("quote", lambda: client.create_quote(params))
            return

        # Sync every order, opted in or not. On opt-in the body carries
        # seel_services with the quote_id and price, which mints the
        # contract and fires contract.created.
        if ORDERS_PATH.match(path):
            params = self._json_body(body)
            if params is None:
                return
            if not self._stamp_merchant(caller, params):
                return
            self._forward("order", lambda: client.create_order(params))
            return

        # Cancel carries no body.
        match = ORDER_CANCEL_PATH.match(path)
        if match:
            order_id = safe_path_param(match.group(1))
            if order_id is None:
                self._respond(400, {"error": "invalid order id in path"})
                return
            self._forward("order cancel", lambda: client.cancel_order(order_id))
            return

        match = FULFILLMENT_PATH.match(path)
        if match:
            order_id = safe_path_param(match.group(1))
            fulfillment_id = safe_path_param(match.group(2))
            if order_id is None or fulfillment_id is None:
                self._respond(400, {"error": "invalid id in path"})
                return
            params = self._json_body(body)
            if params is None:
                return
            self._forward(
                "fulfillment update",
                lambda: client.update_fulfillment(order_id, fulfillment_id, params),
            )
            return

        match = FULFILLMENTS_PATH.match(path)
        if match:
            order_id = safe_path_param(match.group(1))
            if order_id is None:
                self._respond(400, {"error": "invalid order id in path"})
                return
            params = self._json_body(body)
            if params is None:
                return
            self._forward("fulfillment", lambda: client.create_fulfillment(order_id, params))
            return

        match = ORDER_PATH.match(path)
        if match:
            order_id = safe_path_param(match.group(1))
            if order_id is None:
                self._respond(400, {"error": "invalid order id in path"})
                return
            params = self._json_body(body)
            if params is None:
                return
            self._forward("order update", lambda: client.update_order(order_id, params))
            return

        if path == WEBHOOK_PATH:
            signature = self.headers.get("X-Seel-Hmac-SHA256", "")
            # An empty secret is a valid HMAC key, so without this check an
            # unconfigured server authenticates anyone who signs with "".
            if not WEBHOOK_SECRET or not verify_webhook_signature(body, signature, WEBHOOK_SECRET):
                self._respond(401, {"error": "invalid signature"})
                return
            # ACK and flush before doing any work: Seel retries anything not
            # answered with a 200 within 10 seconds.
            self._respond(200, {"ok": True})
            self.wfile.flush()
            try:
                handle_webhook_event(json.loads(body))
            except Exception as exc:  # already ACKed; never let this escape
                print(f"[webhook] processing error: {exc}")
            return

        self._respond(404, {"error": "not found"})


if __name__ == "__main__":
    if not API_KEY:
        print("warning: SEEL_API_KEY not set - quote proxy will fail")
    if not MERCHANT_ID and RETAILER_MERCHANTS is None:
        print("warning: neither SEEL_MERCHANT_ID nor SEEL_RETAILER_MERCHANTS set - "
              "merchant_id is taken from the request body, which only a demo may do")
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    host, port = server.server_address[:2]
    print(f"listening on http://{host}:{port}", flush=True)
    server.serve_forever()
