import assert from "node:assert/strict";
import { test } from "node:test";

import {
  ReportError,
  buildView,
  dayString,
  emptyTotals,
  fillDays,
  isOnline,
  mergeTotals,
  monthStart,
  parseReport,
  parseStoredTotals,
  type MachineRow,
  type Totals,
} from "../src/view.ts";

const NOW = Date.UTC(2026, 9, 29, 12) / 1000;

const totals = (saved: number, jobs = 1): Totals => ({
  jobs,
  runner_secs: jobs * 60,
  saved_vs_github_usd: saved,
  saved_vs_depot_usd: saved,
  own_usd: 0,
});

const slots = (over: Record<string, number> = {}) => ({
  idle: 0,
  busy: 0,
  starting: 0,
  backoff: 0,
  paused: 0,
  offline: 0,
  ...over,
});

const pool = (over = {}) => ({
  name: "linux",
  backend: "apple-container",
  os: "Linux",
  arch: "ARM64",
  cpus: 4,
  labels: ["home"],
  github_label: "github-linux-arm64-4",
  paused: false,
  slots: slots({ idle: 1, busy: 1 }),
  ...over,
});

const report = (over: Record<string, unknown> = {}) => ({
  schema: 2,
  house: "Zack's house",
  version: "0.1.0",
  os: "macOS",
  arch: "ARM64",
  cpus: 8,
  online: true,
  pools: [pool()],
  totals: { all_time: totals(5, 10), days: [{ day: "2026-10-29", ...totals(1.5, 3) }] },
  ...over,
});

const row = (host: string, house: string, over: Partial<MachineRow> = {}): MachineRow => ({
  id: host,
  host,
  house,
  version: "0.1.0",
  os: "macOS",
  arch: "ARM64",
  cpus: 8,
  online: 1,
  last_seen: NOW - 10,
  pools: JSON.stringify([pool()]),
  totals: JSON.stringify({ all_time: totals(5, 10), days: { "2026-10-29": totals(1.5, 3) } }),
  ...over,
});

test("month starts at midnight UTC on the 1st", () => {
  assert.equal(monthStart(NOW), Date.UTC(2026, 9, 1) / 1000);
});

test("machines go offline after missed heartbeats or a clean stop", () => {
  assert.equal(isOnline({ online: 1, last_seen: NOW - 30 }, NOW), true);
  assert.equal(isOnline({ online: 1, last_seen: NOW - 600 }, NOW), false);
  assert.equal(isOnline({ online: 0, last_seen: NOW }, NOW), false);
});

test("a day series covers the window with zeros for quiet days", () => {
  const days = fillDays({ "2026-10-29": totals(1.23456, 3) }, NOW, 14);
  assert.equal(days.length, 14);
  assert.equal(days[0].day, "2026-10-16");
  assert.deepEqual(days[13], { day: "2026-10-29", jobs: 3, saved_vs_github_usd: 1.2346 });
  assert.equal(days[12].jobs, 0);
});

test("accepts a schema 2 report and keeps only the fields the fleet stores", () => {
  const parsed = parseReport(
    report({
      pools: [{ ...pool(), repo: "acme/app", slots: { ...slots({ idle: 2 }), job: { repo: "acme/app" } } }],
      jobs: [{ id: "x", repo: "acme/app" }],
      share: "full",
    }),
  );
  assert.equal(parsed.pools[0].slots.idle, 2);
  assert.equal(parsed.totals.days["2026-10-29"].jobs, 3);
  assert.equal(parsed.online, true);
  const text = JSON.stringify(parsed);
  assert.ok(!text.includes("acme/app") && !text.includes('"jobs":[') && !text.includes("share"));
});

test("rejects reports it cannot trust", () => {
  const bad = (r: unknown, message: RegExp) =>
    assert.throws(() => parseReport(r), (e) => e instanceof ReportError && message.test(e.message));
  bad(null, /object/);
  bad(report({ schema: 1 }), /schema/);
  bad(report({ pools: "nope" }), /pools/);
  bad(report({ pools: [pool({ slots: slots({ idle: -1 }) })] }), /idle/);
  bad(report({ pools: [pool({ name: "x".repeat(200) })] }), /name/);
  bad(report({ totals: { all_time: { ...totals(1), jobs: 1.5 }, days: [] } }), /jobs/);
  bad(report({ totals: { all_time: totals(Number.NaN), days: [] } }), /saved_vs_github_usd/);
  bad(report({ totals: { all_time: totals(1), days: [{ day: "2026-13-40", ...totals(1) }] } }), /YYYY-MM-DD/);
  bad(report({ totals: { all_time: totals(1), days: Array(61).fill({ day: "2026-10-01", ...totals(1) }) } }), /days/);
});

test("a lost ledger doesn't erase history", () => {
  const stored = { all_time: totals(10, 20), days: { "2026-10-28": totals(2, 4), "2026-10-29": totals(3, 5) } };
  const incoming = { all_time: totals(1, 1), days: { "2026-10-29": totals(4, 6), "2026-10-30": totals(1, 1) } };
  const merged = mergeTotals(stored, incoming, NOW);
  assert.equal(merged.all_time.saved_vs_github_usd, 10);
  assert.equal(merged.days["2026-10-28"].jobs, 4);
  assert.equal(merged.days["2026-10-29"].jobs, 6);
  assert.equal(merged.days["2026-10-29"].saved_vs_github_usd, 4);
  assert.ok(merged.days["2026-10-30"]);
});

test("a day's savings can dip when a job costs more than it saved", () => {
  const stored = { all_time: totals(10, 20), days: { "2026-10-29": totals(3, 5) } };
  const incoming = { all_time: totals(9.5, 21), days: { "2026-10-29": totals(2.5, 6) } };
  const merged = mergeTotals(stored, incoming, NOW);
  assert.equal(merged.days["2026-10-29"].saved_vs_github_usd, 2.5);
  assert.equal(merged.all_time.saved_vs_github_usd, 9.5);
});

test("accepts negative savings but not negative time or cost", () => {
  const negative = { ...totals(-0.5, 2) };
  assert.equal(parseReport(report({ totals: { all_time: negative, days: [] } })).totals.all_time.saved_vs_github_usd, -0.5);
  assert.throws(() => parseReport(report({ totals: { all_time: { ...totals(1), own_usd: -1 }, days: [] } })), ReportError);
  assert.throws(() => parseReport(report({ totals: { all_time: { ...totals(1), runner_secs: -1 }, days: [] } })), ReportError);
});

test("old days are dropped after 60 days", () => {
  const old = dayString(NOW - 61 * 86400);
  const edge = dayString(NOW - 59 * 86400);
  const merged = mergeTotals(
    { all_time: emptyTotals(), days: { [old]: totals(1) } },
    { all_time: emptyTotals(), days: { [edge]: totals(1) } },
    NOW,
  );
  assert.deepEqual(Object.keys(merged.days), [edge]);
});

test("a damaged stored row reads as empty", () => {
  assert.deepEqual(parseStoredTotals("not json").all_time, emptyTotals());
  assert.deepEqual(parseStoredTotals("{}").days, {});
});

test("the view ranks houses, sums the month and all time, and shows offline machines as offline", () => {
  const view = buildView("friends", NOW, [
    row("studio", "Zack's house"),
    row("tower", "Dan's garage", {
      last_seen: NOW - 3 * 86400,
      totals: JSON.stringify({
        all_time: totals(50, 100),
        days: { "2026-09-30": totals(40, 90), "2026-10-02": totals(0.5, 1) },
      }),
    }),
  ]);
  // Dan has more all-time but Zack has more this month.
  assert.deepEqual(
    view.houses.map((h) => h.house),
    ["Zack's house", "Dan's garage"],
  );
  assert.equal(view.month.saved_vs_github_usd, 2);
  assert.equal(view.month.jobs, 4);
  assert.equal(view.all_time.saved_vs_github_usd, 55);
  const [studio] = view.houses[0].hosts as any[];
  const [tower] = view.houses[1].hosts as any[];
  assert.equal(studio.online, true);
  assert.equal(studio.pools[0].slots.idle, 1);
  assert.equal(studio.saved_month_usd, 1.5);
  assert.equal(studio.days.length, 30);
  assert.equal(tower.online, false);
  assert.deepEqual(tower.pools[0].slots, slots({ offline: 2 }));
  assert.equal(tower.saved_total_usd, 50);
  assert.equal(tower.saved_month_usd, 0.5);
  assert.equal(view.daily.length, 14);
  assert.equal(view.daily[13].saved_vs_github_usd, 1.5);
});

test("a row with broken JSON doesn't take the view down", () => {
  const view = buildView("f", NOW, [row("a", "A", { pools: "{", totals: "{" })]);
  const [host] = view.houses[0].hosts as any[];
  assert.deepEqual(host.pools, []);
  assert.equal(host.saved_total_usd, 0);
});
