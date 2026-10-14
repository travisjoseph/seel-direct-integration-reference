"""Runs the shared merchant-binding cases against this port.

    python3 -m unittest discover server/python

The cases live in ../binding-cases.json and every port runs them. Each
binding case goes through the real request handler to a stub upstream that
records what it received, so a port that forwards the body merchant_id
fails on the merchant the stub saw, not on a unit-level return value.
"""

import http.client
import json
import pathlib
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

import example_server
from seel_client import SeelClient

SERVER_DIR = pathlib.Path(__file__).resolve().parent.parent
FIXTURE = json.loads((SERVER_DIR / "binding-cases.json").read_text())
PAYLOADS = {
    c["operation"]: c["payload"]
    for c in json.loads((SERVER_DIR / "validation-cases.json").read_text())["cases"]
    if c["name"] in ("quote: complete payload", "order: complete payload")
}
PATHS = {"quote": "/v1/ecommerce/quotes", "order": "/v1/ecommerce/orders"}


class RecordingSeel(BaseHTTPRequestHandler):
    received = []

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        RecordingSeel.received.append(json.loads(body))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
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


class RetailerTableParsing(unittest.TestCase):
    def test_parse_cases(self):
        self.assertGreaterEqual(len(FIXTURE["parse_cases"]), 5)
        for case in FIXTURE["parse_cases"]:
            with self.subTest(case["name"]):
                if case.get("error"):
                    with self.assertRaises(ValueError):
                        example_server.parse_retailer_merchants(case["raw"])
                else:
                    self.assertEqual(
                        example_server.parse_retailer_merchants(case["raw"]), case["expected"]
                    )


class SharedBindingCases(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.upstream = serve(RecordingSeel)
        cls.server = serve(QuietHandler)
        cls.client = SeelClient(
            api_key="dummy", base_url=f"http://127.0.0.1:{cls.upstream.server_address[1]}"
        )

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.upstream.shutdown()

    def request(self, method, path, body, origin):
        conn = http.client.HTTPConnection("127.0.0.1", self.server.server_address[1], timeout=5)
        headers = {"Content-Type": "application/json"}
        if origin is not None:
            headers["Origin"] = origin
        conn.request(method, path, body=body, headers=headers)
        response = conn.getresponse()
        text = response.read().decode()
        conn.close()
        return response, json.loads(text) if text else None

    def configure(self, case):
        return mock.patch.multiple(
            example_server,
            client=self.client,
            MERCHANT_ID=case["single_merchant_id"] or "",
            RETAILER_MERCHANTS=case["retailer_merchants"],
        )

    def test_binding_cases(self):
        self.assertGreaterEqual(len(FIXTURE["binding_cases"]), 5)
        for case in FIXTURE["binding_cases"]:
            with self.subTest(case["name"]), self.configure(case):
                payload = dict(PAYLOADS[case["route"]])
                payload.pop("merchant_id")
                if case["body_merchant_id"] is not None:
                    payload["merchant_id"] = case["body_merchant_id"]
                RecordingSeel.received.clear()

                response, body = self.request(
                    "POST", PATHS[case["route"]], json.dumps(payload), case["origin"]
                )

                self.assertEqual(response.status, case["status"], body)
                self.assertEqual(
                    response.getheader("Access-Control-Allow-Origin"), case["allow_origin"]
                )
                if case["status"] == 200:
                    self.assertEqual(len(RecordingSeel.received), 1)
                    self.assertEqual(RecordingSeel.received[0]["merchant_id"], case["merchant_id"])
                else:
                    self.assertEqual(body, {"error": case["error"]})
                    self.assertEqual(RecordingSeel.received, [], "a refused request reached Seel")

    def test_preflight_echoes_only_a_registered_origin(self):
        table = {"https://dealer-a.example": "m-a"}
        with mock.patch.object(example_server, "RETAILER_MERCHANTS", table):
            known, _ = self.request("OPTIONS", PATHS["quote"], None, "https://dealer-a.example")
            unknown, _ = self.request("OPTIONS", PATHS["quote"], None, "https://other.example")
        self.assertEqual(known.getheader("Access-Control-Allow-Origin"), "https://dealer-a.example")
        self.assertEqual(known.getheader("Vary"), "Origin")
        self.assertIsNone(unknown.getheader("Access-Control-Allow-Origin"))


if __name__ == "__main__":
    unittest.main()
