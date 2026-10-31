// Pure helpers behind the API, kept free of Workers bindings so they can be unit-tested with Node.
//
// Machines send their current state, not events: who they are, what each pool looks like, and
// totals (all time, plus per UTC day). The Worker keeps the latest report per machine, so a report
// can be retried or repeated without double counting.

/** A machine is offline after missing about three 60s heartbeats, or once it says it stopped. */
export const OFFLINE_AFTER_SECS = 180;
export const MAX_POOLS = 32;
/** Days of per-day totals kept per machine. */
export const KEEP_DAYS = 60;
/** Days of history the view gives for each machine, and for the fleet as a whole. */
export const HOST_DAYS = 30;
export const FLEET_DAYS = 14;

export const SLOT_KEYS = ["idle", "busy", "starting", "backoff", "paused", "offline"] as const;
export type SlotCounts = Record<(typeof SLOT_KEYS)[number], number>;

export interface Totals {
  jobs: number;
  runner_secs: number;
  saved_vs_github_usd: number;
  saved_vs_depot_usd: number;
  own_usd: number;
}

export interface Pool {
  name: string;
  backend: string;
  os: string;
  arch: string;
  cpus: number;
  labels: string[];
  github_label: string | null;
  paused: boolean;
  slots: SlotCounts;
}

export interface MachineTotals {
  all_time: Totals;
  /** UTC day (YYYY-MM-DD) -> that day's totals. */
  days: Record<string, Totals>;
}

export interface Report {
  house: string | null;
  version: string;
  os: string;
  arch: string;
  cpus: number;
  online: boolean;
  pools: Pool[];
  totals: MachineTotals;
}

export interface MachineRow {
  id: string;
  host: string;
  house: string;
  version: string;
  os: string;
  arch: string;
  cpus: number;
  online: number;
  last_seen: number;
  pools: string;
  totals: string;
}

export class ReportError extends Error {}

export const emptyTotals = (): Totals => ({
  jobs: 0,
  runner_secs: 0,
  saved_vs_github_usd: 0,
  saved_vs_depot_usd: 0,
  own_usd: 0,
});

const isObject = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

function text(v: unknown, name: string, max: number): string {
  if (typeof v !== "string" || v.length > max) throw new ReportError(`${name} must be a string of at most ${max} characters`);
  return v.trim();
}

function count(v: unknown, name: string, max: number): number {
  if (typeof v !== "number" || !Number.isInteger(v) || v < 0 || v > max) {
    throw new ReportError(`${name} must be an integer between 0 and ${max}`);
  }
  return v;
}

function amount(v: unknown, name: string): number {
  if (typeof v !== "number" || !Number.isFinite(v) || v < 0 || v > 1e12) {
    throw new ReportError(`${name} must be a non-negative number`);
  }
  return v;
}

/** Savings are the hosted price minus what the job cost here, so a day can come out negative. */
function money(v: unknown, name: string): number {
  if (typeof v !== "number" || !Number.isFinite(v) || Math.abs(v) > 1e12) {
    throw new ReportError(`${name} must be a number`);
  }
  return v;
}

function parseTotals(v: unknown, name: string): Totals {
  if (!isObject(v)) throw new ReportError(`${name} must be an object`);
  return {
    jobs: count(v.jobs, `${name}.jobs`, 1e9),
    runner_secs: amount(v.runner_secs, `${name}.runner_secs`),
    saved_vs_github_usd: money(v.saved_vs_github_usd, `${name}.saved_vs_github_usd`),
    saved_vs_depot_usd: money(v.saved_vs_depot_usd, `${name}.saved_vs_depot_usd`),
    own_usd: amount(v.own_usd, `${name}.own_usd`),
  };
}

export function validDay(day: unknown): day is string {
  if (typeof day !== "string" || !/^\d{4}-\d{2}-\d{2}$/.test(day)) return false;
  const d = new Date(`${day}T00:00:00Z`);
  return !Number.isNaN(d.getTime()) && d.toISOString().slice(0, 10) === day;
}

function parsePool(v: unknown, i: number): Pool {
  const at = `pools[${i}]`;
  if (!isObject(v)) throw new ReportError(`${at} must be an object`);
  if (!Array.isArray(v.labels) || v.labels.length > 16) throw new ReportError(`${at}.labels must be a short list`);
  if (!isObject(v.slots)) throw new ReportError(`${at}.slots must be an object`);
  const slots = {} as SlotCounts;
  for (const key of SLOT_KEYS) slots[key] = count(v.slots[key] ?? 0, `${at}.slots.${key}`, 1024);
  const github = v.github_label ?? null;
  return {
    name: text(v.name, `${at}.name`, 60),
    backend: text(v.backend, `${at}.backend`, 30),
    os: text(v.os, `${at}.os`, 20),
    arch: text(v.arch, `${at}.arch`, 20),
    cpus: count(v.cpus ?? 0, `${at}.cpus`, 4096),
    labels: v.labels.map((l, j) => text(l, `${at}.labels[${j}]`, 40)),
    github_label: github === null ? null : text(github, `${at}.github_label`, 60),
    paused: v.paused === true,
    slots,
  };
}

/** Checks a report and cuts it down to the fields the fleet stores; anything else is dropped. */
export function parseReport(raw: unknown): Report {
  if (!isObject(raw)) throw new ReportError("report must be an object");
  if (raw.schema !== 2) throw new ReportError("unsupported report schema; upgrade home-runner");
  if (!Array.isArray(raw.pools) || raw.pools.length > MAX_POOLS) {
    throw new ReportError(`pools must be a list of at most ${MAX_POOLS}`);
  }
  if (!isObject(raw.totals)) throw new ReportError("totals must be an object");
  const days: Record<string, Totals> = {};
  const rawDays = raw.totals.days ?? [];
  if (!Array.isArray(rawDays) || rawDays.length > KEEP_DAYS) {
    throw new ReportError(`totals.days must be a list of at most ${KEEP_DAYS}`);
  }
  for (const [i, d] of rawDays.entries()) {
    if (!isObject(d) || !validDay(d.day)) throw new ReportError(`totals.days[${i}].day must be YYYY-MM-DD`);
    days[d.day] = parseTotals(d, `totals.days[${i}]`);
  }
  const house = typeof raw.house === "string" && raw.house.trim() ? raw.house.trim().slice(0, 60) : null;
  return {
    house,
    version: text(raw.version ?? "", "version", 40),
    os: text(raw.os ?? "", "os", 20),
    arch: text(raw.arch ?? "", "arch", 20),
    cpus: count(raw.cpus ?? 0, "cpus", 4096),
    online: raw.online !== false,
    pools: raw.pools.map(parsePool),
    totals: { all_time: parseTotals(raw.totals.all_time, "totals.all_time"), days },
  };
}

export function dayString(nowSecs: number): string {
  return new Date(nowSecs * 1000).toISOString().slice(0, 10);
}

export function monthStart(nowSecs: number): number {
  const d = new Date(nowSecs * 1000);
  return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1) / 1000;
}

export function isOnline(m: Pick<MachineRow, "online" | "last_seen">, nowSecs: number): boolean {
  return m.online === 1 && nowSecs - m.last_seen <= OFFLINE_AFTER_SECS;
}

/** The snapshot that has seen more jobs. A machine only ever adds jobs, so fewer means lost history. */
const fuller = (stored: Totals, incoming: Totals): Totals => (incoming.jobs >= stored.jobs ? incoming : stored);

const sumTotals = (a: Totals, b: Totals): Totals => ({
  jobs: a.jobs + b.jobs,
  runner_secs: a.runner_secs + b.runner_secs,
  saved_vs_github_usd: a.saved_vs_github_usd + b.saved_vs_github_usd,
  saved_vs_depot_usd: a.saved_vs_depot_usd + b.saved_vs_depot_usd,
  own_usd: a.own_usd + b.own_usd,
});

/** Reads what was stored; a damaged row reads as empty rather than taking the view down. */
export function parseStoredTotals(json: string): MachineTotals {
  try {
    const v = JSON.parse(json);
    if (!isObject(v)) throw new Error("not an object");
    const days: Record<string, Totals> = {};
    if (isObject(v.days)) {
      for (const [day, t] of Object.entries(v.days)) if (validDay(day)) days[day] = parseTotals(t, day);
    }
    return { all_time: isObject(v.all_time) ? parseTotals(v.all_time, "all_time") : emptyTotals(), days };
  } catch {
    return { all_time: emptyTotals(), days: {} };
  }
}

/**
 * Folds a new report into what is stored. A machine only ever adds jobs, so for the all-time
 * figure and for each day the snapshot with more jobs wins whole: a machine whose ledger was
 * lost or replaced doesn't take its history with it, and savings can still go down by a cent.
 */
export function mergeTotals(stored: MachineTotals, incoming: MachineTotals, nowSecs: number): MachineTotals {
  const oldest = dayString(nowSecs - (KEEP_DAYS - 1) * 86400);
  const days: Record<string, Totals> = {};
  for (const [day, t] of Object.entries(stored.days)) if (day >= oldest) days[day] = t;
  for (const [day, t] of Object.entries(incoming.days)) {
    if (day >= oldest) days[day] = days[day] ? fuller(days[day], t) : t;
  }
  return { all_time: fuller(stored.all_time, incoming.all_time), days };
}

export const round = (v: number): number => Math.round(v * 10000) / 10000;

const roundTotals = (t: Totals): Totals => ({
  jobs: t.jobs,
  runner_secs: round(t.runner_secs),
  saved_vs_github_usd: round(t.saved_vs_github_usd),
  saved_vs_depot_usd: round(t.saved_vs_depot_usd),
  own_usd: round(t.own_usd),
});

/** The last `days` UTC days, oldest first, with zeros for days without jobs. */
export function fillDays(byDay: Record<string, Totals>, nowSecs: number, days: number) {
  const out = [];
  for (let i = days - 1; i >= 0; i--) {
    const day = dayString(nowSecs - i * 86400);
    const t = byDay[day];
    out.push({ day, jobs: t?.jobs ?? 0, saved_vs_github_usd: round(t?.saved_vs_github_usd ?? 0) });
  }
  return out;
}

/** What an offline machine last said it was doing is no longer true: all its slots are offline. */
function offlinePools(pools: Pool[]): Pool[] {
  return pools.map((p) => {
    const total = SLOT_KEYS.reduce((n, k) => n + p.slots[k], 0);
    const slots = Object.fromEntries(SLOT_KEYS.map((k) => [k, 0])) as SlotCounts;
    slots.offline = total;
    return { ...p, slots };
  });
}

function parsePools(json: string): Pool[] {
  try {
    const v = JSON.parse(json);
    return Array.isArray(v) ? v.map(parsePool) : [];
  } catch {
    return [];
  }
}

export function buildView(fleet: string, nowSecs: number, machines: MachineRow[]) {
  const month = dayString(nowSecs).slice(0, 7);
  const houses = new Map<
    string,
    { house: string; saved_month_usd: number; jobs_month: number; saved_total_usd: number; hosts: unknown[] }
  >();
  let monthTotals = emptyTotals();
  let allTime = emptyTotals();
  const fleetDays: Record<string, Totals> = {};

  for (const m of [...machines].sort((a, b) => a.host.localeCompare(b.host))) {
    const online = isOnline(m, nowSecs);
    const totals = parseStoredTotals(m.totals);
    const pools = parsePools(m.pools);
    let host = emptyTotals();
    for (const [day, t] of Object.entries(totals.days)) {
      fleetDays[day] = fleetDays[day] ? sumTotals(fleetDays[day], t) : t;
      if (day.startsWith(month)) host = sumTotals(host, t);
    }
    monthTotals = sumTotals(monthTotals, host);
    allTime = sumTotals(allTime, totals.all_time);

    let h = houses.get(m.house);
    if (!h) {
      h = { house: m.house, saved_month_usd: 0, jobs_month: 0, saved_total_usd: 0, hosts: [] };
      houses.set(m.house, h);
    }
    h.saved_month_usd += host.saved_vs_github_usd;
    h.jobs_month += host.jobs;
    h.saved_total_usd += totals.all_time.saved_vs_github_usd;
    h.hosts.push({
      host: m.host,
      online,
      last_seen: m.last_seen,
      version: m.version,
      os: m.os,
      arch: m.arch,
      cpus: m.cpus,
      pools: online ? pools : offlinePools(pools),
      saved_month_usd: round(host.saved_vs_github_usd),
      saved_total_usd: round(totals.all_time.saved_vs_github_usd),
      jobs_month: host.jobs,
      jobs_total: totals.all_time.jobs,
      days: fillDays(totals.days, nowSecs, HOST_DAYS),
    });
  }

  return {
    fleet,
    now: nowSecs,
    month: { ...roundTotals(monthTotals), repos: 0 },
    all_time: roundTotals(allTime),
    houses: [...houses.values()]
      .map((h) => ({
        ...h,
        saved_month_usd: round(h.saved_month_usd),
        saved_total_usd: round(h.saved_total_usd),
      }))
      .sort((a, b) => b.saved_month_usd - a.saved_month_usd || a.house.localeCompare(b.house)),
    daily: fillDays(fleetDays, nowSecs, FLEET_DAYS),
  };
}
