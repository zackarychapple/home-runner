import type { FleetView, Host, Pool } from "./types.ts";

export type Period = "month" | "all";

/** A machine with the house it belongs to. */
export interface Node extends Host {
  house: string;
}

export interface SlotSummary {
  /** Idle: a runner is registered and waiting for a job. */
  open: number;
  busy: number;
  /** Starting, backing off, drained or offline. */
  other: number;
  total: number;
}

export function nodes(view: FleetView): Node[] {
  return view.houses.flatMap((h) => h.hosts.map((host) => ({ ...host, house: h.house })));
}

export const saved = (n: Node, period: Period): number =>
  period === "month" ? n.saved_month_usd : n.saved_total_usd;

/** Most saved first; ties go to the machine that has been around longer, then by name. */
export function rank(list: Node[], period: Period): Node[] {
  return [...list].sort(
    (a, b) =>
      saved(b, period) - saved(a, period) ||
      b.saved_total_usd - a.saved_total_usd ||
      a.host.localeCompare(b.host),
  );
}

export function slotSummary(pools: Pool[]): SlotSummary {
  const s = { open: 0, busy: 0, other: 0, total: 0 };
  for (const p of pools) {
    const c = p.slots;
    s.open += c.idle;
    s.busy += c.busy;
    s.other += c.starting + c.backoff + c.paused + c.offline;
  }
  s.total = s.open + s.busy + s.other;
  return s;
}

export interface Headline {
  savedMonth: number;
  savedTotal: number;
  jobsMonth: number;
  nodes: number;
  online: number;
  open: number;
  slots: number;
}

export function headline(view: FleetView): Headline {
  const list = nodes(view);
  const slots = list.map((n) => slotSummary(n.pools));
  return {
    savedMonth: view.month.saved_vs_github_usd,
    savedTotal: view.all_time.saved_vs_github_usd,
    jobsMonth: view.month.jobs,
    nodes: list.length,
    online: list.filter((n) => n.online).length,
    open: slots.reduce((n, s) => n + s.open, 0),
    slots: slots.reduce((n, s) => n + s.total, 0),
  };
}

/** What kind of runner a pool is, in the words people use: "macOS · ARM64 · Tart". */
export function poolKind(p: Pool): string {
  const backend: Record<string, string> = {
    "apple-container": "Apple container",
    tart: "Tart VM",
    docker: "Docker",
    host: "Host",
  };
  return [p.os, p.arch, backend[p.backend] ?? p.backend].filter(Boolean).join(" · ");
}
