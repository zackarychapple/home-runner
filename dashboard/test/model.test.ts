import assert from "node:assert/strict";
import { test } from "node:test";

import { ago, plural, shortDay, usd } from "../src/format.ts";
import { parseKeyFromHash } from "../src/key.ts";
import { headline, nodes, poolKind, rank, slotSummary } from "../src/model.ts";
import type { FleetView, Host, Pool } from "../src/types.ts";

const slots = (over = {}) => ({ idle: 0, busy: 0, starting: 0, backoff: 0, paused: 0, offline: 0, ...over });
const pool = (over: Partial<Pool> = {}): Pool => ({
  name: "linux",
  backend: "apple-container",
  os: "Linux",
  arch: "ARM64",
  cpus: 4,
  labels: [],
  github_label: null,
  paused: false,
  slots: slots({ idle: 1 }),
  ...over,
});
const host = (name: string, month: number, total: number, over: Partial<Host> = {}): Host => ({
  host: name,
  online: true,
  last_seen: 0,
  version: "0.1.0",
  os: "macOS",
  arch: "ARM64",
  cpus: 8,
  pools: [pool()],
  saved_month_usd: month,
  saved_total_usd: total,
  jobs_month: 1,
  jobs_total: 1,
  days: [],
  ...over,
});
const totals = (saved: number, jobs: number) => ({
  jobs,
  runner_secs: 0,
  saved_vs_github_usd: saved,
  saved_vs_depot_usd: 0,
  own_usd: 0,
});
const view = (): FleetView => ({
  fleet: "f",
  now: 0,
  month: totals(3, 9),
  all_time: totals(30, 90),
  daily: [],
  houses: [
    { house: "A", saved_month_usd: 1, jobs_month: 1, saved_total_usd: 20, hosts: [host("a1", 1, 20)] },
    {
      house: "B",
      saved_month_usd: 2,
      jobs_month: 2,
      saved_total_usd: 10,
      hosts: [
        host("b1", 2, 5),
        host("b2", 0, 5, { online: false, pools: [pool({ slots: slots({ offline: 2 }) })] }),
      ],
    },
  ],
});

test("flattens houses into nodes that remember their house", () => {
  assert.deepEqual(nodes(view()).map((n) => `${n.house}/${n.host}`), ["A/a1", "B/b1", "B/b2"]);
});

test("ranks by the chosen period, breaking ties by all-time then name", () => {
  const list = nodes(view());
  assert.deepEqual(rank(list, "month").map((n) => n.host), ["b1", "a1", "b2"]);
  assert.deepEqual(rank(list, "all").map((n) => n.host), ["a1", "b1", "b2"]);
});

test("open slots are idle runners; offline slots are not open", () => {
  assert.deepEqual(
    slotSummary([pool({ slots: slots({ idle: 2, busy: 1, starting: 1, backoff: 1, paused: 1 }) }), pool({ slots: slots({ offline: 3 }) })]),
    { open: 2, busy: 1, other: 6, total: 9 },
  );
});

test("headline totals the fleet", () => {
  assert.deepEqual(headline(view()), {
    savedMonth: 3,
    savedTotal: 30,
    jobsMonth: 9,
    nodes: 3,
    online: 2,
    open: 2,
    slots: 4,
  });
});

test("describes pool kinds in plain words", () => {
  assert.equal(poolKind(pool()), "Linux · ARM64 · Apple container");
  assert.equal(poolKind(pool({ os: "macOS", backend: "tart" })), "macOS · ARM64 · Tart VM");
  assert.equal(poolKind(pool({ backend: "mystery" })), "Linux · ARM64 · mystery");
});

test("formats money, time and counts", () => {
  assert.equal(usd(0), "$0.00");
  assert.equal(usd(0.004), "<$0.01");
  assert.equal(usd(7.694), "$7.69");
  assert.equal(usd(12345.6), "$12,346");
  assert.equal(ago(100, 98), "just now");
  assert.equal(ago(100, 40), "1m ago");
  assert.equal(ago(10000, 0), "2h ago");
  assert.equal(plural(1, "job"), "1 job");
  assert.equal(plural(1200, "job"), "1,200 jobs");
  assert.equal(shortDay("2026-10-29"), "Oct 29");
});

test("reads the view key from a link fragment", () => {
  assert.equal(parseKeyFromHash("#k=abc123"), "abc123");
  assert.equal(parseKeyFromHash("#x=1&k=a%2Bb"), "a+b");
  assert.equal(parseKeyFromHash("#k="), null);
  assert.equal(parseKeyFromHash(""), null);
});
