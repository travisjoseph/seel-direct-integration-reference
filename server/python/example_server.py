"""Example backend for the direct-integration path. Stdlib only.

Two routes:
  POST /api/seel/quote  - browser quote proxy: attaches the server-side API
                          key and forwards to Seel's Quote API (the widget
                          never sees the key)
  POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
                          events: verifies HMAC, ACKs 200 fast, then hands
                          off for internal fan-out

Run:
  SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... python3 example_server.py

To drive the widget demo against a live sandbox, two steps - this server
doesn't serve the demo page:
  1. run this server
  2. in widget/demo.html, replace the mock quoteFetcher with
     configure({ quoteEndpoint: "http://localhost:8787/api/seel/quote" })
"""

import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from seel_client import SANDBOX_BASE_URL, SeelAPIError, SeelClient, verify_webhook_signature

PORT = int(os.environ.get("PORT", "8787"))
API_KEY = os.environ.get("SEEL_API_KEY", "")
WEBHOOK_SECRET = os.environ.get("SEEL_WEBHOOK_SECRET", "")
BASE_URL = os.environ.get("SEEL_BASE_URL", SANDBOX_BASE_URL)
# Program values from Seel onboarding. When set, the proxy stamps them into
# every quote request, so storefront code stays identical across programs.
MERCHANT_ID = os.environ.get("SEEL_MERCHANT_ID", "")
QUOTE_TYPE = os.environ.get("SEEL_QUOTE_TYPE", "")

client = SeelClient(api_key=API_KEY, base_url=BASE_URL)


def handle_webhook_event(event: dict) -> None:
    """Internal fan-out. Map merchant_id/order_id to your own retailer code
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

    def _read_body(self) -> bytes:
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = 0
        return self.rfile.read(length) if length > 0 else b""

    def do_POST(self):
        body = self._read_body()

        if self.path == "/api/seel/quote":
            try:
                params = json.loads(body)
            except json.JSONDecodeError:
                self._respond(400, {"error": "request body must be JSON"})
                return
            if MERCHANT_ID:
                params["merchant_id"] = MERCHANT_ID
            if QUOTE_TYPE:
                params["type"] = QUOTE_TYPE
            try:
                self._respond(200, client.create_quote(params))
            except SeelAPIError as exc:
                # Forward Seel's status and error body - it names the
                # offending field.
                self._respond(exc.status, exc.body if isinstance(exc.body, dict) else {"error": str(exc)})
            except Exception:
                self._respond(502, {"error": "upstream quote request failed"})
            return

        if self.path == "/webhooks/seel":
            signature = self.headers.get("X-Seel-Hmac-SHA256", "")
            if not verify_webhook_signature(body, signature, WEBHOOK_SECRET):
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
    print(f"listening on http://localhost:{PORT}")
    ThreadingHTTPServer(("", PORT), Handler).serve_forever()
