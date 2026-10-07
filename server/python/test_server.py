"""Behavior tests for example_server and the client's transport edge cases.

    python3 -m unittest discover server/python

A mock Seel upstream and the example server run in-process on free ports,
so nothing here needs network access or credentials. Requests go over a
raw socket so the tests control the exact bytes on the wire (chunked
framing, a non-ASCII header), which http.client would normalize away.
"""

import base64
import hashlib
import hmac
import http.client
import json
import pathlib
import select
import socket
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import example_server
from seel_client import SeelClient, verify_webhook_signature

SECRET = "test-secret"
CAP = 1048576  # the 1 MiB request body cap, fixed by the cross-port spec

FIXTURE = json.loads(
    (pathlib.Path(__file__).resolve().parent.parent / "validation-cases.json").read_text()
)
COMPLETE_ORDER = next(c for c in FIXTURE["cases"] if c["name"] == "order: complete payload")["payload"]


class MockSeel(BaseHTTPRequestHandler):
    """Picks the upstream behavior from the path so each test states its own."""

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path.endswith("/O-EMPTY/cancel"):
            self._reply(200, b"")
        elif self.path.endswith("/O-TEXT/cancel"):
            self._reply(200, b"OK", "text/plain")
        elif self.path.endswith("/O-SLOW/cancel"):
            time.sleep(1.5)
            self._reply(200, b"{}")
        elif self.path == "/v1/ecommerce/orders":
            self._reply(200, b"[]")
        else:
            self._reply(200, b'{"ok": true}')

    def _reply(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class QuietHandler(example_server.Handler):
    def log_message(self, *args):
        pass


def serve(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def sign(body: bytes) -> str:
    return base64.b64encode(hmac.new(SECRET.encode(), body, hashlib.sha256).digest()).decode()


def raw_post(port, path, body=b"", headers=(), timeout=5):
    """POST exact bytes and return (status, parsed body).

    Returns (None, None) when the server closed the connection without
    answering. The body goes out in slices and sending stops as soon as a
    reply is readable, because a server that rejects an oversized body
    answers before the body has finished arriving.
    """
    header_lines = "".join(f"{k}: {v}\r\n" for k, v in headers)
    if not any(k.lower() in ("content-length", "transfer-encoding") for k, _ in headers):
        header_lines += f"Content-Length: {len(body)}\r\n"
    head = f"POST {path} HTTP/1.1\r\nHost: localhost\r\n{header_lines}\r\n".encode("latin-1")
    sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    try:
        sock.sendall(head)
        for i in range(0, len(body), 65536):
            if select.select([sock], [], [], 0)[0]:
                break
            try:
                sock.sendall(body[i : i + 65536])
            except OSError:
                break
        response = http.client.HTTPResponse(sock)
        try:
            response.begin()
        except (http.client.RemoteDisconnected, ConnectionResetError):
            return None, None
        text = response.read().decode()
        return response.status, json.loads(text) if text else None
    finally:
        sock.close()


class SignatureVerification(unittest.TestCase):
    def test_valid_signature(self):
        body = b'{"type": "contract.created", "id": "evt_1"}'
        self.assertTrue(verify_webhook_signature(body, sign(body), SECRET))

    def test_non_ascii_signature_is_rejected_not_an_error(self):
        # hmac.compare_digest raises TypeError on non-ASCII str input, which
        # used to escape the handler and drop the connection.
        self.assertFalse(verify_webhook_signature(b"{}", "\xe9\xe9", SECRET))


class ServerBehavior(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.upstream = serve(MockSeel)
        cls.server = serve(QuietHandler)
        cls.port = cls.server.server_address[1]
        example_server.client = SeelClient(
            api_key="dummy",
            base_url=f"http://127.0.0.1:{cls.upstream.server_address[1]}",
            timeout=0.5,
        )
        example_server.WEBHOOK_SECRET = SECRET

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.upstream.shutdown()

    def post(self, path, body=b"", headers=(), timeout=5):
        return raw_post(self.port, path, body, headers, timeout)

    # -- webhook receiver -------------------------------------------------

    def test_non_ascii_signature_header_is_401(self):
        status, body = self.post(
            "/webhooks/seel", b"{}", [("X-Seel-Hmac-SHA256", "\xe9\xe9")]
        )
        self.assertEqual((status, body), (401, {"error": "invalid signature"}))

    def test_chunked_webhook_body_is_decoded_and_verified(self):
        payload = b'{"type": "contract.created", "id": "evt_1"}'
        chunked = (
            f"{len(payload) - 10:x}\r\n".encode() + payload[:-10] + b"\r\n"
            + b"a\r\n" + payload[-10:] + b"\r\n"
            + b"0\r\n\r\n"
        )
        status, body = self.post(
            "/webhooks/seel", chunked,
            [("Transfer-Encoding", "chunked"), ("X-Seel-Hmac-SHA256", sign(payload))],
        )
        self.assertEqual((status, body), (200, {"ok": True}))

    # -- body cap ---------------------------------------------------------

    def test_content_length_over_cap_is_413(self):
        status, body = self.post("/v1/ecommerce/quotes", b"x" * (CAP + 1))
        self.assertEqual((status, body), (413, {"error": f"request body exceeds {CAP} bytes"}))

    def test_content_length_at_cap_is_read(self):
        payload = b'{"a": "' + b"x" * (CAP - 9) + b'"}'
        self.assertEqual(len(payload), CAP)
        status, body = self.post("/v1/ecommerce/quotes", payload)
        self.assertEqual(status, 400)  # read in full, then failed validation
        self.assertIn("missing required field merchant_id", body["problems"])

    def test_chunked_body_over_cap_is_413(self):
        half = b"x" * (CAP // 2 + 1)
        chunk = f"{len(half):x}\r\n".encode() + half + b"\r\n"
        status, body = self.post(
            "/v1/ecommerce/quotes", chunk + chunk + b"0\r\n\r\n", [("Transfer-Encoding", "chunked")]
        )
        self.assertEqual((status, body), (413, {"error": f"request body exceeds {CAP} bytes"}))

    def test_malformed_chunk_size_is_400(self):
        status, body = self.post(
            "/v1/ecommerce/quotes", b"zz\r\n{}\r\n0\r\n\r\n", [("Transfer-Encoding", "chunked")]
        )
        self.assertEqual(status, 400)

    # -- upstream outcome mapping ----------------------------------------

    def test_empty_2xx_body_is_an_empty_object(self):
        status, body = self.post("/v1/ecommerce/orders/O-EMPTY/cancel")
        self.assertEqual((status, body), (200, {}))

    def test_non_json_2xx_body_passes_status_with_raw_text(self):
        status, body = self.post("/v1/ecommerce/orders/O-TEXT/cancel")
        self.assertEqual((status, body), (200, {"seel_raw_body": "OK"}))

    def test_create_order_with_list_body_is_409_not_minted(self):
        order = dict(COMPLETE_ORDER, seel_services=[{"type": "acme-wfp", "quote_id": "q-1", "price": 0.98}])
        status, body = self.post("/v1/ecommerce/orders", json.dumps(order).encode())
        self.assertEqual(status, 409)
        self.assertEqual(body["seel_response"], [])
        self.assertIn("no contract was minted", body["error"])

    def test_timeout_is_504(self):
        status, body = self.post("/v1/ecommerce/orders/O-SLOW/cancel")
        self.assertEqual(status, 504)
        self.assertEqual(
            body,
            {"error": "timed out waiting for Seel; the request may have been processed, "
                      "check before retrying"},
        )


if __name__ == "__main__":
    unittest.main()
