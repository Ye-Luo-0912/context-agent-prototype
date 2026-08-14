const { test } = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");
const parse = require(path.join(__dirname, "..", "parse.js"));

test("positive durations still parse", () => {
  assert.equal(parse("1h"), 3600000);
  assert.equal(parse(".5ms"), 0.5);
  assert.equal(parse("2m"), 120000);
});

test("negative durations parse as negative milliseconds", () => {
  assert.equal(parse("-5"), -5);
  assert.equal(parse("-.5ms"), -0.5);
  assert.equal(parse("-0.5ms"), -0.5);
});
