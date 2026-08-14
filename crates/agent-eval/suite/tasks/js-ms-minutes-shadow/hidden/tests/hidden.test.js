const { test } = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");
const parse = require(path.join(__dirname, "..", "parse.js"));

test("minutes uses the 60000 constant, not the match array", () => {
  assert.equal(parse("2m"), 120000);
  assert.equal(parse("1 minutes"), 60000);
});

test("other units still parse", () => {
  assert.equal(parse("2s"), 2000);
  assert.equal(parse("1h"), 3600000);
  assert.equal(parse("5"), 5);
});
