import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createApp } from "../server.js";

let server;
let base;

before(async () => {
  server = createApp();
  await new Promise((resolve) => server.listen(0, resolve));
  base = `http://127.0.0.1:${server.address().port}`;
});

after(() => {
  server.closeAllConnections();
  server.close();
});

async function checkout(code) {
  const response = await fetch(`${base}/api/checkout`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(code === undefined ? {} : { code }),
  });
  return { status: response.status, body: await response.json() };
}

const page = () =>
  readFile(new URL("../public/index.html", import.meta.url), "utf8");

test("checkout without a code charges subtotal plus shipping", async () => {
  const { status, body } = await checkout();
  assert.equal(status, 200);
  assert.equal(body.subtotal_cents, 8000);
  assert.equal(body.total_cents, 8500);
});

test("SAVE20 takes 20% off the subtotal", async () => {
  const { status, body } = await checkout("SAVE20");
  assert.equal(status, 200);
  assert.equal(body.discount_cents, 1600);
  assert.equal(body.total_cents, 6900);
});

test("codes are case-insensitive and trimmed", async () => {
  const { body } = await checkout("  save20 ");
  assert.equal(body.total_cents, 6900);
});

test("FREESHIP removes shipping", async () => {
  const { status, body } = await checkout("FREESHIP");
  assert.equal(status, 200);
  assert.equal(body.shipping_cents, 0);
  assert.equal(body.total_cents, 8000);
});

test("BULK3 takes $3 off each unit of an item bought 3 or more at a time", async () => {
  const { status, body } = await checkout("BULK3");
  assert.equal(status, 200);
  assert.equal(body.discount_cents, 900);
  assert.equal(body.total_cents, 7600);
});

test("a blank code is the same as no code", async () => {
  const { status, body } = await checkout("   ");
  assert.equal(status, 200);
  assert.equal(body.total_cents, 8500);
});

test("an unknown code is rejected with a message", async () => {
  const { status, body } = await checkout("BOGUS");
  assert.equal(status, 400);
  assert.match(body.error, /not a valid code/i);
});

test("an expired code is rejected as expired", async () => {
  const { status, body } = await checkout("SUMMER10");
  assert.equal(status, 400);
  assert.match(body.error, /expired/i);
});

test("the response names the applied code", async () => {
  const { body } = await checkout("save20");
  assert.equal(body.code, "SAVE20");
});

test("the checkout page has a discount field and apply button", async () => {
  const html = await page();
  assert.match(html, /id="discount-code"/);
  assert.match(html, /id="apply-discount"/);
});

test("the order summary has a discount line", async () => {
  assert.match(await page(), /id="discount-line"/);
});
