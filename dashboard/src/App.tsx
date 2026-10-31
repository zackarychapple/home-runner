import { useEffect, useMemo, useRef, useState } from "preact/hooks";

import { FLEET_URL, Unauthorized, fetchFleet } from "./api.ts";
import { ago, plural, usd } from "./format.ts";
import { forgetKey, loadKey, saveKey } from "./key.ts";
import { headline, poolKind, rank, saved, slotSummary, nodes, type Node, type Period } from "./model.ts";
import { Sparkline } from "./Sparkline.tsx";
import type { FleetView, Pool } from "./types.ts";

const POLL_MS = 15_000;
const MEDALS = ["🥇", "🥈", "🥉"];

export function App() {
  const [key, setKey] = useState<string | null>(() => loadKey());
  const [rejected, setRejected] = useState(false);
  const signOut = (wasRejected = false) => {
    forgetKey();
    setKey(null);
    setRejected(wasRejected);
  };
  if (!key) {
    return (
      <KeyForm
        rejected={rejected}
        onKey={(k) => {
          saveKey(k);
          setKey(k.trim());
          setRejected(false);
        }}
      />
    );
  }
  return <Board apiKey={key} onSignOut={signOut} />;
}

function KeyForm({ rejected, onKey }: { rejected: boolean; onKey: (key: string) => void }) {
  const [value, setValue] = useState("");
  return (
    <main class="gate">
      <h1>home-runner fleet</h1>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (value.trim()) onKey(value);
        }}
      >
        <label for="key">View key</label>
        <input id="key" type="password" autocomplete="current-password" autofocus value={value} onInput={(e) => setValue(e.currentTarget.value)} />
        {rejected && <p class="error" role="alert">That key didn't work.</p>}
        <button type="submit">Open the leaderboard</button>
        <p class="hint">Ask whoever runs the fleet for the key, or open the link they shared. It stays in this browser.</p>
      </form>
    </main>
  );
}

function useFleet(apiKey: string, onRejected: () => void) {
  const [view, setView] = useState<FleetView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fetchedAt, setFetchedAt] = useState(0);
  const rejected = useRef(onRejected);
  rejected.current = onRejected;

  useEffect(() => {
    let live: AbortController | null = null;
    const load = async () => {
      live?.abort();
      live = new AbortController();
      try {
        setView(await fetchFleet(apiKey, live.signal));
        setError(null);
        setFetchedAt(Math.floor(Date.now() / 1000));
      } catch (e) {
        if (e instanceof Unauthorized) rejected.current();
        else if (!(e instanceof DOMException && e.name === "AbortError")) setError(e instanceof Error ? e.message : String(e));
      }
    };
    const poll = () => document.visibilityState === "visible" && void load();
    void load();
    const timer = setInterval(poll, POLL_MS);
    document.addEventListener("visibilitychange", poll);
    return () => {
      live?.abort();
      clearInterval(timer);
      document.removeEventListener("visibilitychange", poll);
    };
  }, [apiKey]);
  return { view, error, fetchedAt };
}

function useNow(): number {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    const t = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000);
    return () => clearInterval(t);
  }, []);
  return now;
}

function Board({ apiKey, onSignOut }: { apiKey: string; onSignOut: (rejected?: boolean) => void }) {
  const { view, error, fetchedAt } = useFleet(apiKey, () => onSignOut(true));
  const [period, setPeriod] = useState<Period>(() => (localStorage.getItem("hr-period") === "all" ? "all" : "month"));
  const now = useNow();
  const choose = (p: Period) => {
    setPeriod(p);
    localStorage.setItem("hr-period", p);
  };
  const ranked = useMemo(() => (view ? rank(nodes(view), period) : []), [view, period]);
  const top = ranked.length ? saved(ranked[0], period) : 0;

  return (
    <main>
      <header class="top">
        <div>
          <h1>{view?.fleet ?? "home-runner"} fleet</h1>
          <p class="sub">
            {fetchedAt ? `Updated ${ago(now, fetchedAt)}` : "Loading…"}
            {error && <span class="error"> · {error} Retrying…</span>}
          </p>
        </div>
        <button class="link" onClick={() => onSignOut()}>Forget key</button>
      </header>

      {!FLEET_URL && <p class="error" role="alert">This dashboard was built without VITE_FLEET_URL.</p>}

      {view && (
        <>
          <Tiles view={view} />
          <section aria-labelledby="board">
            <div class="board-head">
              <h2 id="board">Leaderboard</h2>
              <fieldset class="seg">
                <legend class="sr">Rank by</legend>
                {(["month", "all"] as const).map((p) => (
                  <label class={period === p ? "on" : ""}>
                    <input type="radio" name="period" checked={period === p} onChange={() => choose(p)} />
                    {p === "month" ? "This month" : "All time"}
                  </label>
                ))}
              </fieldset>
            </div>
            {ranked.length === 0 ? (
              <p class="empty">No machines have reported yet. Join one with <code>home-runner fleet join</code>.</p>
            ) : (
              <ol class="rows">
                {ranked.map((n, i) => (
                  <Row node={n} rank={i} period={period} top={top} now={now} />
                ))}
              </ol>
            )}
          </section>
          <p class="foot">
            Savings are estimates: what the same runner time would have cost on GitHub-hosted runners at list price, as each machine
            works it out.
          </p>
        </>
      )}
    </main>
  );
}

function Tiles({ view }: { view: FleetView }) {
  const h = headline(view);
  const tiles = [
    { label: "Saved this month", value: usd(h.savedMonth), note: "est." },
    { label: "Saved all time", value: usd(h.savedTotal), note: "est." },
    { label: "Jobs this month", value: h.jobsMonth.toLocaleString("en-US"), note: "" },
    { label: "Machines online", value: `${h.online} / ${h.nodes}`, note: "" },
    { label: "Open slots", value: `${h.open} of ${h.slots}`, note: "ready for a job" },
  ];
  return (
    <section class="tiles" aria-label="Fleet totals">
      {tiles.map((t) => (
        <div class="tile">
          <div class="tile-label">{t.label}</div>
          <div class="tile-value">{t.value}</div>
          {t.note && <div class="tile-note">{t.note}</div>}
        </div>
      ))}
    </section>
  );
}

function Row({ node, rank, period, top, now }: { node: Node; rank: number; period: Period; top: number; now: number }) {
  const value = saved(node, period);
  const width = top > 0 ? Math.max((value / top) * 100, value > 0 ? 2 : 0) : 0;
  return (
    <li class={`row${node.online ? "" : " off"}`}>
      <div class="rank" aria-label={`Rank ${rank + 1}`}>{MEDALS[rank] ?? rank + 1}</div>
      <div class="who">
        <div class="name">
          <span class="host">{node.host}</span>
          <span class={`status ${node.online ? "on" : ""}`}>
            <span class="dot" aria-hidden="true" />
            {node.online ? "online" : `offline · seen ${ago(now, node.last_seen)}`}
          </span>
        </div>
        <div class="meta">
          {node.house} · {node.os} {node.arch} · {plural(node.cpus, "CPU")}
        </div>
        <ul class="pools">
          {node.pools.map((p) => (
            <PoolLine pool={p} />
          ))}
        </ul>
      </div>
      <div class="money">
        <div class="amount">{usd(value)}</div>
        <div class="bar" aria-hidden="true">
          <span style={{ width: `${width}%` }} />
        </div>
        <div class="meta">
          {period === "month" ? `${usd(node.saved_total_usd)} all time` : `${usd(node.saved_month_usd)} this month`}
          {" · "}
          {plural(period === "month" ? node.jobs_month : node.jobs_total, "job")}
        </div>
      </div>
      <div class="trend">
        <Sparkline days={node.days} />
        <div class="meta">last 30 days</div>
      </div>
    </li>
  );
}

/** One pool: its kind, and a pip per slot. Ring = open, solid = busy, dash = starting, backing off, drained or offline. */
function PoolLine({ pool }: { pool: Pool }) {
  const s = slotSummary([pool]);
  const pips = [
    ...Array<string>(s.open).fill("open"),
    ...Array<string>(s.busy).fill("busy"),
    ...Array<string>(s.other).fill("other"),
  ];
  return (
    <li class="pool">
      <span class="pips" role="img" aria-label={`${s.open} open, ${s.busy} busy, ${s.other} not ready`}>
        {pips.map((kind) => (
          <span class={`pip ${kind}`} />
        ))}
      </span>
      <span class="pool-name">{pool.name}</span>
      <span class="pool-kind">{poolKind(pool)}</span>
      <span class="pool-count">
        {s.open} open{s.busy ? ` · ${s.busy} busy` : ""}
        {pool.paused ? " · drained" : ""}
      </span>
    </li>
  );
}
