// The shape of GET /v1/fleet (see ../../fleet-worker/src/view.ts).

export interface SlotCounts {
  idle: number;
  busy: number;
  starting: number;
  backoff: number;
  paused: number;
  offline: number;
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

export interface Day {
  day: string;
  jobs: number;
  saved_vs_github_usd: number;
}

export interface Host {
  host: string;
  online: boolean;
  last_seen: number;
  version: string;
  os: string;
  arch: string;
  cpus: number;
  pools: Pool[];
  saved_month_usd: number;
  saved_total_usd: number;
  jobs_month: number;
  jobs_total: number;
  days: Day[];
}

export interface House {
  house: string;
  saved_month_usd: number;
  jobs_month: number;
  saved_total_usd: number;
  hosts: Host[];
}

export interface Totals {
  jobs: number;
  runner_secs: number;
  saved_vs_github_usd: number;
  saved_vs_depot_usd: number;
  own_usd: number;
}

export interface FleetView {
  fleet: string;
  now: number;
  month: Totals;
  all_time: Totals;
  houses: House[];
  daily: Day[];
}
