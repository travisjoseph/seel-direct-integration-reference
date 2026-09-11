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
    SeelClient,
    SeelContractNotMintedError,
    validate_merchant_payload,
    validate_order_payload,
    validate_quote_payload,
)

FIXTURE = json.loads(
    (pathlib.Path(__file__).resolve().parent.parent / "validation-cases.json").read_text()
)
CASES = FIXTURE["cases"]
CONTRACT_CASES = FIXTURE["contract_cases"]
UNCOVERED = FIXTURE["uncovered_requests"]

VALIDATORS = {
    "quote": validate_quote_payload,
    "order": validate_order_payload,
    "merchant": validate_merchant_payload,
}


class SharedValidationCases(unittest.TestCase):
    def test_fixture_is_populated(self):
        """The fixture is the single point of failure for cross-port drift,
        so an emptied or defanged one has to fail rather than pass green."""
        self.assertTrue(CASES and CONTRACT_CASES and UNCOVERED, "fixture is empty")
        for case in CASES:
            if not case.get("expect_clean"):
                self.assertTrue(
                    case.get("expect_contains"), f"{case['name']}: expect_contains is empty"
                )

    def test_shared_cases(self):
        for case in CASES:
            with self.subTest(case=case["name"]):
                problems = VALIDATORS[case["operation"]](case["payload"])
                joined = "; ".join(problems)
                if case.get("expect_clean"):
                    self.assertEqual(problems, [], f"expected no problems, got {problems}")
                else:
                    self.assertTrue(problems, "expected problems, got none")
                    for fragment in case["expect_contains"]:
                        self.assertIn(fragment, joined)


class SharedContractCases(unittest.TestCase):
    """The post-condition check on Create Order must agree across ports."""

    def test_contract_cases(self):
        for case in CONTRACT_CASES:
            with self.subTest(case=case["name"]):
                try:
                    SeelClient._check_contract_minted(case["request"], case["response"])
                    minted = True
                except SeelContractNotMintedError:
                    minted = False
                self.assertEqual(minted, case["expect_minted"])

    def test_check_only_runs_when_coverage_was_attached(self):
        for case in UNCOVERED:
            with self.subTest(case=case["name"]):
                services = case["request"].get("seel_services")
                checked = bool(isinstance(services, list) and services)
                self.assertEqual(checked, case["expect_checked"])


if __name__ == "__main__":
    unittest.main()
