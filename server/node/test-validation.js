/**
 * Runs the shared validation cases against this port.
 *
 *   node --test server/node
 *
 * The cases live in ../validation-cases.json and are read by the test suite
 * in every language port. They exist to catch drift: the ports must make
 * the same accept/reject decision and report the same field paths, and four
 * hand-written suites would encode divergence rather than catch it.
 */

"use strict";

const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");

const {
  SeelClient,
  SeelContractNotMintedError,
  validateMerchantPayload,
  validateOrderPayload,
  validateQuotePayload,
} = require("./seel-client");

const { cases, contract_cases: contractCases, uncovered_requests: uncovered } = JSON.parse(
  fs.readFileSync(path.join(__dirname, "..", "validation-cases.json"), "utf8")
);

const validators = {
  quote: validateQuotePayload,
  order: validateOrderPayload,
  merchant: validateMerchantPayload,
};

// The fixture is the single point of failure for cross-port drift, so an
// emptied or defanged one has to fail rather than pass green.
test("fixture is populated", () => {
  assert.ok(cases.length && contractCases.length && uncovered.length, "fixture is empty");
  for (const testCase of cases) {
    if (!testCase.expect_clean) {
      assert.ok(testCase.expect_contains?.length, `${testCase.name}: expect_contains is empty`);
    }
  }
});

for (const testCase of cases) {
  test(testCase.name, () => {
    const problems = validators[testCase.operation](testCase.payload);
    const joined = problems.join("; ");
    if (testCase.expect_clean) {
      assert.deepStrictEqual(problems, [], `expected no problems, got ${joined}`);
    } else {
      assert.ok(problems.length > 0, "expected problems, got none");
      for (const fragment of testCase.expect_contains) {
        assert.ok(joined.includes(fragment), `expected "${fragment}" in: ${joined}`);
      }
    }
  });
}

// The post-condition check on Create Order must agree across ports.
for (const testCase of contractCases) {
  test(`contract: ${testCase.name}`, () => {
    let minted = true;
    try {
      SeelClient._checkContractMinted(testCase.request, testCase.response);
    } catch (exc) {
      assert.ok(exc instanceof SeelContractNotMintedError, `unexpected ${exc}`);
      minted = false;
    }
    assert.strictEqual(minted, testCase.expect_minted);
  });
}

for (const testCase of uncovered) {
  test(`gating: ${testCase.name}`, () => {
    const services = testCase.request.seel_services;
    const checked = Array.isArray(services) && services.length > 0;
    assert.strictEqual(checked, testCase.expect_checked);
  });
}
