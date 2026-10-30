// home-runner fleet: machines in different houses report heartbeats and job records here, and
// read fleet-wide totals back. See ../README.md ("Fleet") for setup.
//
//   POST /v1/fleets   (admin)    {name}                  create a fleet
//   POST /v1/invites  (admin)    {fleet, uses?, days?}    invite code for machines to join
//   POST /v1/join                {invite, host, house}    -> {token, fleet}
//   POST /v1/report   (machine)  heartbeat + new job records
//   GET  /v1/fleet    (machine)  totals, houses, machines and what they are running
//   GET  /v1/jobs     (machine)  ?since=<unix>&limit=<n>  recent jobs across the fleet

import {
  MAX_JOBS_PER_REPORT,
  SHARE_LEVELS,
  buildView,
  invalidRecord,
  inviteCode,
  monthStart,
  type DayRow,
  type HouseTotalsRow,
  type JobRecord,
  type MachineRow,
  type TotalsRow,
} from "./view.ts";

export interface Env {
  DB: D1Database;
  ADMIN_TOKEN?: string;
}

class HttpError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });

const now = () => Math.floor(Date.now() / 1000);

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    try {
      return await route(request, env);
    } catch (e) {
      if (e instanceof HttpError) return json({ error: e.message }, e.status);
      console.error(e);
      return json({ error: "internal error" }, 500);
    }
  },
} satisfies ExportedHandler<Env>;

async function route(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const key = `${request.method} ${url.pathname.replace(/\/+$/, "")}`;
  switch (key) {
    case "POST /v1/fleets":
      return createFleet(request, env);
    case "POST /v1/invites":
      return createInvite(request, env);
    case "POST /v1/join":
      return join(request, env);
    case "POST /v1/report":
      return report(request, env);
    case "GET /v1/fleet":
      return fleetView(request, env);
    case "GET /v1/jobs":
      return jobs(request, env, url);
    case "GET ":
    case "GET /":
      return json({ service: "home-runner fleet" });
    default:
      throw new HttpError(404, "not found");
  }
}

async function body<T>(request: Request, maxBytes = 2 * 1024 * 1024): Promise<T> {
  const text = await request.text();
  if (text.length > maxBytes) throw new HttpError(413, "request too large");
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new HttpError(400, "invalid JSON");
  }
}

const str = (v: unknown, name: string, max = 100): string => {
  if (typeof v !== "string" || v.trim() === "" || v.length > max) {
    throw new HttpError(400, `${name} is required (at most ${max} characters)`);
  }
  return v.trim();
};

async function sha256(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

const randomBytes = (n: number) => crypto.getRandomValues(new Uint8Array(n));

async function requireAdmin(request: Request, env: Env) {
  const given = bearer(request);
  if (!env.ADMIN_TOKEN || !given) throw new HttpError(401, "admin token required");
  // Compare hashes so lengths match and timing says nothing about the token.
  const [a, b] = await Promise.all([sha256(given), sha256(env.ADMIN_TOKEN)]);
  const enc = new TextEncoder();
  if (!crypto.subtle.timingSafeEqual(enc.encode(a), enc.encode(b))) {
    throw new HttpError(401, "admin token required");
  }
}

function bearer(request: Request): string | null {
  const h = request.headers.get("authorization") ?? "";
  return h.startsWith("Bearer ") ? h.slice(7).trim() : null;
}

async function requireMachine(request: Request, env: Env): Promise<MachineRow & { fleet_id: string }> {
  const token = bearer(request);
  if (!token) throw new HttpError(401, "machine token required");
  const row = await env.DB.prepare("SELECT * FROM machines WHERE token_hash = ?")
    .bind(await sha256(token))
    .first<MachineRow & { fleet_id: string }>();
  if (!row) throw new HttpError(401, "unknown machine token; join the fleet again");
  return row;
}

async function createFleet(request: Request, env: Env) {
  await requireAdmin(request, env);
  const { name } = await body<{ name: unknown }>(request);
  const fleet = { id: crypto.randomUUID(), name: str(name, "name") };
  const existing = await env.DB.prepare("SELECT id FROM fleets WHERE name = ?").bind(fleet.name).first();
  if (existing) throw new HttpError(409, "a fleet with that name exists");
  await env.DB.prepare("INSERT INTO fleets (id, name, created_at) VALUES (?, ?, ?)")
    .bind(fleet.id, fleet.name, now())
    .run();
  return json(fleet, 201);
}

async function createInvite(request: Request, env: Env) {
  await requireAdmin(request, env);
  const b = await body<{ fleet: unknown; uses?: unknown; days?: unknown }>(request);
  const fleet = await env.DB.prepare("SELECT id FROM fleets WHERE name = ?")
    .bind(str(b.fleet, "fleet"))
    .first<{ id: string }>();
  if (!fleet) throw new HttpError(404, "no such fleet");
  const uses = Math.min(Math.max(Number(b.uses ?? 10) | 0, 1), 1000);
  const days = Math.min(Math.max(Number(b.days ?? 7), 0.01), 365);
  const code = inviteCode(randomBytes(8));
  const expires_at = now() + Math.round(days * 86400);
  await env.DB.prepare(
    "INSERT INTO invites (code, fleet_id, created_at, expires_at, uses_left) VALUES (?, ?, ?, ?, ?)",
  )
    .bind(code, fleet.id, now(), expires_at, uses)
    .run();
  return json({ code, expires_at, uses }, 201);
}

async function join(request: Request, env: Env) {
  const b = await body<{ invite: unknown; host: unknown; house: unknown }>(request, 4096);
  const code = str(b.invite, "invite", 20).toUpperCase();
  const host = str(b.host, "host", 40);
  const house = str(b.house, "house", 60);
  // Claim one use atomically so a code can't be overspent by parallel joins.
  const invite = await env.DB.prepare(
    `UPDATE invites SET uses_left = uses_left - 1
     WHERE code = ? AND uses_left > 0 AND expires_at > ?
     RETURNING fleet_id`,
  )
    .bind(code, now())
    .first<{ fleet_id: string }>();
  if (!invite) throw new HttpError(403, "invite code is invalid, used up or expired");
  const fleet = await env.DB.prepare("SELECT name FROM fleets WHERE id = ?")
    .bind(invite.fleet_id)
    .first<{ name: string }>();

  const token = `hrf_${btoa(String.fromCharCode(...randomBytes(32)))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "")}`;
  // Joining again from the same host replaces its token; its job history stays.
  await env.DB.prepare(
    `INSERT INTO machines (id, fleet_id, token_hash, host, house, joined_at)
     VALUES (?, ?, ?, ?, ?, ?)
     ON CONFLICT (fleet_id, host) DO UPDATE SET token_hash = excluded.token_hash, house = excluded.house`,
  )
    .bind(crypto.randomUUID(), invite.fleet_id, await sha256(token), host, house, now())
    .run();
  return json({ token, fleet: fleet?.name ?? "" }, 201);
}

interface Report {
  house?: unknown;
  version?: unknown;
  os?: unknown;
  arch?: unknown;
  cpus?: unknown;
  share?: unknown;
  online?: unknown;
  pools?: unknown;
  jobs?: unknown;
}

async function report(request: Request, env: Env) {
  const machine = await requireMachine(request, env);
  const r = await body<Report>(request);
  const jobs = Array.isArray(r.jobs) ? (r.jobs as unknown[]) : [];
  if (jobs.length > MAX_JOBS_PER_REPORT) {
    throw new HttpError(413, `at most ${MAX_JOBS_PER_REPORT} jobs per report`);
  }
  const share = SHARE_LEVELS.includes(r.share as never) ? (r.share as string) : "repos";
  const pools = JSON.stringify(Array.isArray(r.pools) ? r.pools : []);
  if (pools.length > 256 * 1024) throw new HttpError(413, "pools too large");
  const house = typeof r.house === "string" && r.house.trim() ? r.house.trim().slice(0, 60) : machine.house;

  const statements = [
    env.DB.prepare(
      `UPDATE machines SET house = ?, version = ?, os = ?, arch = ?, cpus = ?, share = ?, online = ?,
       last_seen = ?, pools = ? WHERE id = ?`,
    ).bind(
      house,
      String(r.version ?? "").slice(0, 40),
      String(r.os ?? "").slice(0, 20),
      String(r.arch ?? "").slice(0, 20),
      Number(r.cpus) | 0,
      share,
      r.online === false ? 0 : 1,
      now(),
      pools,
      machine.id,
    ),
  ];
  const insert = env.DB.prepare(
    `INSERT OR IGNORE INTO jobs (machine_id, id, fleet_id, house, started_at, duration_secs, repo, result,
     saved_vs_github_usd, saved_vs_depot_usd, own_usd, record) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
  );
  let rejected = 0;
  for (const raw of jobs) {
    if (invalidRecord(raw)) {
      rejected++;
      continue;
    }
    const j = raw as JobRecord;
    // The machine can't claim another host's jobs.
    const record = { ...j, host: machine.host };
    statements.push(
      insert.bind(
        machine.id,
        j.id,
        machine.fleet_id,
        house,
        Math.floor(j.started_at),
        Number(j.duration_secs) || 0,
        typeof j.repo === "string" ? j.repo.slice(0, 200) : null,
        typeof j.result === "string" ? j.result.slice(0, 40) : null,
        Number(j.saved_vs_github_usd) || 0,
        Number(j.saved_vs_depot_usd) || 0,
        (Number(j.electricity_usd) || 0) + (Number(j.self_hosted_fee_usd) || 0),
        JSON.stringify(record),
      ),
    );
  }
  const results = await env.DB.batch(statements);
  const accepted = results.slice(1).reduce((n, res) => n + (res.meta.changes ?? 0), 0);
  return json({ accepted, rejected });
}

async function fleetView(request: Request, env: Env) {
  const machine = await requireMachine(request, env);
  const t = now();
  const since = monthStart(t);
  const fourteenDays = t - 14 * 86400;
  const fid = machine.fleet_id;
  const [fleet, totals, machines, houses, days] = await env.DB.batch([
    env.DB.prepare("SELECT name FROM fleets WHERE id = ?").bind(fid),
    env.DB.prepare(
      `SELECT count(*) AS jobs, sum(duration_secs) AS runner_secs, sum(saved_vs_github_usd) AS saved_vs_github_usd,
       sum(saved_vs_depot_usd) AS saved_vs_depot_usd, sum(own_usd) AS own_usd, count(DISTINCT repo) AS repos
       FROM jobs WHERE fleet_id = ? AND started_at >= ?`,
    ).bind(fid, since),
    env.DB.prepare(
      "SELECT id, host, house, share, version, os, arch, cpus, online, last_seen, pools FROM machines WHERE fleet_id = ?",
    ).bind(fid),
    env.DB.prepare(
      `SELECT house, count(*) AS jobs, sum(saved_vs_github_usd) AS saved FROM jobs
       WHERE fleet_id = ? AND started_at >= ? GROUP BY house`,
    ).bind(fid, since),
    env.DB.prepare(
      `SELECT strftime('%Y-%m-%d', started_at, 'unixepoch') AS day, count(*) AS jobs, sum(saved_vs_github_usd) AS saved
       FROM jobs WHERE fleet_id = ? AND started_at >= ? GROUP BY day`,
    ).bind(fid, fourteenDays),
  ]);
  return json(
    buildView(
      (fleet.results[0] as { name: string } | undefined)?.name ?? "",
      t,
      totals.results[0] as TotalsRow,
      machines.results as unknown as MachineRow[],
      houses.results as unknown as HouseTotalsRow[],
      days.results as unknown as DayRow[],
    ),
  );
}

async function jobs(request: Request, env: Env, url: URL) {
  const machine = await requireMachine(request, env);
  const since = Number(url.searchParams.get("since") ?? 0) || 0;
  const limit = Math.min(Math.max(Number(url.searchParams.get("limit") ?? 200) | 0, 1), 1000);
  const rows = await env.DB.prepare(
    "SELECT house, record FROM jobs WHERE fleet_id = ? AND started_at >= ? ORDER BY started_at DESC LIMIT ?",
  )
    .bind(machine.fleet_id, since, limit)
    .all<{ house: string; record: string }>();
  return json({
    jobs: rows.results.map((r) => ({ ...JSON.parse(r.record), house: r.house })),
  });
}
