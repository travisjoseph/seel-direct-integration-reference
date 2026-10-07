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

  Platform proxy - the platform holds one API key for every retailer on it,
  retailers point at the platform instead of at Seel, and the platform
  resolves which merchant each request belongs to. Retailers hold no Seel
  credentials at all. See authenticate_caller() and resolve_merchant_id()
  below, which are where a platform starts. They are not the whole job:
  order_id comes off the URL and is never checked against the caller, so
  nothing here stops one retailer touching another's order. That mapping
  belongs to the platform - see the README.

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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
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
API_KEY = os.environ.get("SEEL_API_KEY", "")
WEBHOOK_SECRET = os.environ.get("SEEL_WEBHOOK_SECRET", "")
BASE_URL = os.environ.get("SEEL_BASE_URL", SANDBOX_BASE_URL)
# Program values from Seel onboarding. When set, the proxy stamps them into
# every quote request, so storefront code stays identical across programs.
MERCHANT_ID = os.environ.get("SEEL_MERCHANT_ID", "")
QUOTE_TYPE = os.environ.get("SEEL_QUOTE_TYPE", "")

client = SeelClient(api_key=API_KEY, base_url=BASE_URL)

# Routes mirror Seel's real paths, prefix included, so a caller already
# written against Seel moves onto a platform by changing the base URL and
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


def authenticate_caller(headers) -> bool:
    """Decide whether the caller may use this proxy.

    This demo accepts everyone, which is only safe because it holds a
    sandbox key and listens on localhost.

    A platform MUST replace this. Retailers authenticate to the platform
    with platform credentials - they never receive a Seel API key, because
    one key covers every retailer on the platform and would let any holder
    act as any other. A real implementation returns the caller's identity
    rather than a bool, and resolve_merchant_id() takes it - changing both
    signatures is part of the work.
    """
    return True


def resolve_merchant_id(params: dict) -> str:
    """Return the merchant ID this request belongs to.

    Single retailer: SEEL_MERCHANT_ID is set once in the environment and
    stamped onto everything, so storefront and pipeline code carry no
    program-specific values.

    Platform proxy: leave SEEL_MERCHANT_ID unset and look the merchant up
    from the authenticated caller instead. Deriving it from the caller
    rather than trusting the request body is what stops one retailer
    quoting or ordering against another's merchant ID.

    The body fallback below is a demo default so the unconfigured server
    still runs. It is not safe on a real platform: any caller can name any
    merchant. Replace it before anyone but you can reach this.
    """
    if MERCHANT_ID:
        return MERCHANT_ID
    return params.get("merchant_id", "")


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
        self.send_header("Access-Control-Allow-Origin", "*")  # demo only; lock down in prod
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_OPTIONS(self):  # CORS preflight for the demo page
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Allow-Methods", "POST, OPTIONS")
        self.end_headers()

    def _read_body(self):
        """Read the request body, or answer 413/400 and return None.

        Content-Length is checked before reading and a chunked body is
        capped as it streams, so neither framing can push more than
        MAX_BODY_BYTES into memory.
        """
        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            try:
                body = self._read_chunked()
            except ValueError:
                self._respond(400, {"error": "malformed chunked request body"})
                return None
        else:
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                length = 0
            body = None if length > MAX_BODY_BYTES else self.rfile.read(max(length, 0))
        if body is None:
            self._respond(413, {"error": f"request body exceeds {MAX_BODY_BYTES} bytes"})
        return body

    def _read_chunked(self):
        """Decode a Transfer-Encoding: chunked body (RFC 9112 section 7.1).

        http.server does not decode it, so a signed webhook sent chunked
        would otherwise verify against an empty body and 401. Returns None
        once the declared chunks exceed the cap, raises ValueError on
        malformed framing.
        """
        body = bytearray()
        while True:
            size = int(self.rfile.readline(65537).split(b";", 1)[0].strip(), 16)
            if size < 0:
                raise ValueError("negative chunk size")
            if size == 0:
                break
            if len(body) + size > MAX_BODY_BYTES:
                return None
            chunk = self.rfile.read(size)
            if len(chunk) != size or self.rfile.read(2) != b"\r\n":
                raise ValueError("truncated chunk")
            body += chunk
        while self.rfile.readline(65537) not in (b"\r\n", b"\n", b""):
            pass  # trailer fields carry nothing this server uses
        return bytes(body)

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

        if path != WEBHOOK_PATH and not authenticate_caller(self.headers):
            self._respond(401, {"error": "unauthorized"})
            return

        if path == QUOTE_PATH:
            params = self._json_body(body)
            if params is None:
                return
            merchant_id = resolve_merchant_id(params)
            if merchant_id:
                params["merchant_id"] = merchant_id
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
            merchant_id = resolve_merchant_id(params)
            if merchant_id:
                params["merchant_id"] = merchant_id
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
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    host, port = server.server_address[:2]
    print(f"listening on http://{host}:{port}", flush=True)
    server.serve_forever()
