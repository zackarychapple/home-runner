import assert from "node:assert/strict";
import { test } from "node:test";

import { bearer, isMachineToken, newMachineToken, safeEqual, sha256 } from "../src/auth.ts";

test("safeEqual matches equal secrets only", async () => {
  assert.equal(await safeEqual("correct horse", "correct horse"), true);
  assert.equal(await safeEqual("correct horse", "correct horsf"), false);
  assert.equal(await safeEqual("short", "a much longer secret"), false);
  assert.equal(await safeEqual("", "x"), false);
});

test("sha256 is the usual hex digest", async () => {
  assert.equal(await sha256("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
});

test("reads a bearer token", () => {
  assert.equal(bearer(new Headers({ authorization: "Bearer abc " })), "abc");
  assert.equal(bearer(new Headers({ authorization: "Bearer " })), null);
  assert.equal(bearer(new Headers({ authorization: "Basic abc" })), null);
  assert.equal(bearer(new Headers()), null);
});

test("machine tokens are recognisable and url-safe", () => {
  const token = newMachineToken(new Uint8Array(32).fill(251));
  assert.ok(isMachineToken(token));
  assert.match(token, /^hrf_[A-Za-z0-9_-]+$/);
  assert.ok(!isMachineToken("some-view-key"));
});
