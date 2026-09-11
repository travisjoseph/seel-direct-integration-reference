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
  validateMerchantPayload,
  validateOrderPayload,
  validateQuotePayload,
} = require("./seel-client");

const { cases } = JSON.parse(
  fs.readFileSync(path.join(__dirname, "..", "validation-cases.json"), "utf8")
);

const validators = {
  quote: validateQuotePayload,
  order: validateOrderPayload,
  merchant: validateMerchantPayload,
};

for (const testCase of cases) {
  test(testCase.name, () => {
    const problems = validators[testCase.operation](testCase.payload);
    const joined = problems.join("; ");
    if (testCase.expect_clean) {
      assert.deepStrictEqual(problems, [], `expected no problems, got ${joined}`);
    } else {
      for (const fragment of testCase.expect_contains) {
        assert.ok(joined.includes(fragment), `expected "${fragment}" in: ${joined}`);
      }
    }
  });
}
