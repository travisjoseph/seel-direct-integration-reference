"""Runs the shared validation cases against this port.

    python3 -m unittest discover server/python

The cases live in ../validation-cases.json and are read by the test suite
in every language port. They exist to catch drift: the ports must make the
same accept/reject decision and report the same field paths, and four
hand-written suites would encode divergence rather than catch it.
"""

import json
import pathlib
import unittest

from seel_client import (
    validate_merchant_payload,
    validate_order_payload,
    validate_quote_payload,
)

CASES = json.loads(
    (pathlib.Path(__file__).resolve().parent.parent / "validation-cases.json").read_text()
)["cases"]

VALIDATORS = {
    "quote": validate_quote_payload,
    "order": validate_order_payload,
    "merchant": validate_merchant_payload,
}


class SharedValidationCases(unittest.TestCase):
    def test_shared_cases(self):
        for case in CASES:
            with self.subTest(case=case["name"]):
                problems = VALIDATORS[case["operation"]](case["payload"])
                joined = "; ".join(problems)
                if case.get("expect_clean"):
                    self.assertEqual(problems, [], f"expected no problems, got {problems}")
                else:
                    for fragment in case["expect_contains"]:
                        self.assertIn(fragment, joined)


if __name__ == "__main__":
    unittest.main()
