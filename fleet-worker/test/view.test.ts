import assert from "node:assert/strict";
import { test } from "node:test";

import { buildView, fillDays, invalidRecord, inviteCode, isOnline, monthStart } from "../src/view.ts";

const NOW = Date.UTC(2026, 9, 29, 12) / 1000;

test("month starts at midnight UTC on the 1st", () => {
  assert.equal(monthStart(NOW), Date.UTC(2026, 9, 1) / 1000);
});

test("machines go offline after missed heartbeats or a clean stop", () => {
  assert.equal(isOnline({ online: 1, last_seen: NOW - 30 }, NOW), true);
  assert.equal(isOnline({ online: 1, last_seen: NOW - 600 }, NOW), false);
  assert.equal(isOnline({ online: 0, last_seen: NOW }, NOW), false);
});

test("daily series covers 14 days with zeros", () => {
  const days = fillDays([{ day: "2026-10-29", jobs: 3, saved: 1.23456 }], NOW);
  assert.equal(days.length, 14);
  assert.equal(days[0].day, "2026-10-16");
  assert.deepEqual(days[13], { day: "2026-10-29", jobs: 3, saved_vs_github_usd: 1.2346 });
  assert.equal(days[12].jobs, 0);
});

test("view groups machines into houses and hides stale live jobs", () => {
  const pools = JSON.stringify([
    { name: "linux", slots: [{ slot: 0, phase: "busy", job: { repo: "acme/app" } }] },
  ]);
  const machine = (host: string, house: string, last_seen: number) => ({
    id: host,
    host,
    house,
    share: "repos",
    version: "0.1.0",
    os: "macOS",
    arch: "ARM64",
    cpus: 8,
    online: 1,
    last_seen,
    pools,
  });
  const view = buildView(
    "friends",
    NOW,
    { jobs: 4, runner_secs: 3600, saved_vs_github_usd: 3, saved_vs_depot_usd: 2, own_usd: 0.1, repos: 2 },
    [machine("studio", "Zack's house", NOW - 10), machine("tower", "Dan's garage", NOW - 3 * 86400)],
    [
      { house: "Zack's house", jobs: 3, saved: 2.5 },
      { house: "Dan's garage", jobs: 1, saved: 0.5 },
    ],
    [],
  );
  assert.equal(view.houses[0].house, "Zack's house");
  const [studio] = view.houses[0].hosts as any[];
  const [tower] = view.houses[1].hosts as any[];
  assert.equal(studio.online, true);
  assert.equal(studio.pools[0].slots[0].job.repo, "acme/app");
  assert.equal(tower.online, false);
  assert.equal(tower.pools[0].slots[0].phase, "offline");
  assert.equal(tower.pools[0].slots[0].job, null);
  assert.equal(view.month.saved_vs_github_usd, 3);
});

test("rejects malformed job records", () => {
  assert.equal(invalidRecord({ id: "r1", started_at: NOW }), null);
  assert.equal(invalidRecord({ id: "", started_at: NOW }), "bad id");
  assert.equal(invalidRecord({ id: "r1" }), "bad started_at");
  assert.equal(invalidRecord({ id: "r1", started_at: NOW, pad: "x".repeat(20000) }), "too large");
});

test("invite codes are readable", () => {
  assert.match(inviteCode(new Uint8Array([0, 1, 2, 3, 4, 5, 6, 7])), /^[A-Z2-9]{4}-[A-Z2-9]{4}$/);
});
