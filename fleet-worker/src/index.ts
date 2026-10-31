// home-runner fleet: machines report what they are and how much they have saved; the dashboard
// and `home-runner fleet status` read it back. No job details are ever sent or stored. See
// ../README.md ("Fleet") for setup.
//
//   POST   /v1/join          {key, host, house}   -> {token, fleet}     (ENROLL_KEY)
//   POST   /v1/report        snapshot (schema 2)  -> {ok}               (machine token)
//   GET    /v1/fleet                              -> the leaderboard    (VIEW_KEY or machine token)
//   DELETE /v1/machines/:host                     -> {removed}          (ADMIN_TOKEN)

import { bearer, isMachineToken, newMachineToken, safeEqual, sha256 } from "./auth.ts";
import {
  ReportError,
  buildView,
  mergeTotals,
  parseReport,
  parseStoredTotals,
  type MachineRow,
} from "./view.ts";

export interface Env {
  DB: D1Database;
  FLEET_NAME?: string;
  ENROLL_KEY?: string;
  VIEW_KEY?: string;
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

// Requests carry a bearer key, never cookies, so any origin may call this API: a page can't
// use a visitor's credentials against it, and the dashboard's address isn't known in advance.
const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, DELETE, OPTIONS",
  "access-control-allow-headers": "authorization, content-type",
  "access-control-max-age": "86400",
};

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...CORS },
  });

const now = () => Math.floor(Date.now() / 1000);

const DEFAULT_FLEET = "default";

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method === "OPTIONS") return new Response(null, { status: 204, headers: CORS });
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
  const path = url.pathname.replace(/\/+$/, "");
  const key = `${request.method} ${path}`;
  switch (key) {
    case "POST /v1/join":
      return join(request, env);
    case "POST /v1/report":
      return report(request, env);
    case "GET /v1/fleet":
      return fleetView(request, env);
    case "GET ":
    case "GET /":
      return json({ service: "home-runner fleet" });
  }
  const machine = /^\/v1\/machines\/([^/]+)$/.exec(path);
  if (request.method === "DELETE" && machine) {
    return removeMachine(request, env, decodeURIComponent(machine[1]));
  }
  throw new HttpError(404, "not found");
}

async function body<T>(request: Request, maxBytes = 256 * 1024): Promise<T> {
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

const randomBytes = (n: number) => crypto.getRandomValues(new Uint8Array(n));

/** Checks the bearer token against a Worker secret. An unset secret never matches. */
async function matchesSecret(request: Request, secret: string | undefined): Promise<boolean> {
  const given = bearer(request.headers);
  return Boolean(given && secret && (await safeEqual(given, secret)));
}

async function requireAdmin(request: Request, env: Env) {
  if (!(await matchesSecret(request, env.ADMIN_TOKEN))) throw new HttpError(401, "admin token required");
}

async function machineFor(request: Request, env: Env): Promise<(MachineRow & { fleet_id: string }) | null> {
  const token = bearer(request.headers);
  if (!token || !isMachineToken(token)) return null;
  return env.DB.prepare("SELECT * FROM machines WHERE token_hash = ?")
    .bind(await sha256(token))
    .first<MachineRow & { fleet_id: string }>();
}

async function requireMachine(request: Request, env: Env) {
  if (!bearer(request.headers)) throw new HttpError(401, "machine token required");
  const row = await machineFor(request, env);
  if (!row) throw new HttpError(401, "unknown machine token; join the fleet again");
  return row;
}

async function join(request: Request, env: Env) {
  const b = await body<{ key: unknown; host: unknown; house: unknown }>(request, 4096);
  const host = str(b.host, "host", 40);
  const house = str(b.house, "house", 60);
  if (!env.ENROLL_KEY) throw new HttpError(403, "this fleet isn't accepting new machines");
  if (!(await safeEqual(str(b.key, "key", 200), env.ENROLL_KEY))) {
    throw new HttpError(403, "enroll key is wrong");
  }
  const fleet = env.FLEET_NAME || "home-runner";
  const token = newMachineToken(randomBytes(32));
  // Joining again from the same host replaces its token; its totals stay.
  await env.DB.batch([
    env.DB.prepare("INSERT OR IGNORE INTO fleets (id, name, created_at) VALUES (?, ?, ?)").bind(
      DEFAULT_FLEET,
      fleet,
      now(),
    ),
    env.DB.prepare(
      `INSERT INTO machines (id, fleet_id, token_hash, host, house, joined_at)
       VALUES (?, ?, ?, ?, ?, ?)
       ON CONFLICT (fleet_id, host) DO UPDATE SET token_hash = excluded.token_hash, house = excluded.house`,
    ).bind(crypto.randomUUID(), DEFAULT_FLEET, await sha256(token), host, house, now()),
  ]);
  return json({ token, fleet }, 201);
}

async function report(request: Request, env: Env) {
  const machine = await requireMachine(request, env);
  let r;
  try {
    r = parseReport(await body(request));
  } catch (e) {
    if (e instanceof ReportError) throw new HttpError(400, e.message);
    throw e;
  }
  const t = now();
  const totals = mergeTotals(parseStoredTotals(machine.totals ?? "{}"), r.totals, t);
  await env.DB.prepare(
    `UPDATE machines SET house = ?, version = ?, os = ?, arch = ?, cpus = ?, online = ?,
     last_seen = ?, pools = ?, totals = ? WHERE id = ?`,
  )
    .bind(
      r.house ?? machine.house,
      r.version,
      r.os,
      r.arch,
      r.cpus,
      r.online ? 1 : 0,
      t,
      JSON.stringify(r.pools),
      JSON.stringify(totals),
      machine.id,
    )
    .run();
  return json({ ok: true });
}

async function fleetView(request: Request, env: Env) {
  // A machine's own token can read the fleet too, for `home-runner fleet status` and the app.
  const token = bearer(request.headers);
  if (token && isMachineToken(token)) await requireMachine(request, env);
  else if (!(await matchesSecret(request, env.VIEW_KEY))) throw new HttpError(401, "view key required");
  const rows = await env.DB.prepare(
    "SELECT id, host, house, version, os, arch, cpus, online, last_seen, pools, totals FROM machines",
  ).all<MachineRow>();
  return json(buildView(env.FLEET_NAME || "home-runner", now(), rows.results));
}

async function removeMachine(request: Request, env: Env, host: string) {
  await requireAdmin(request, env);
  const res = await env.DB.prepare("DELETE FROM machines WHERE host = ?").bind(host).run();
  if (!res.meta.changes) throw new HttpError(404, "no such machine");
  return json({ removed: host });
}
