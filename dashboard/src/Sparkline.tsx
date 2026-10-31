import { plural, shortDay, usd } from "./format.ts";
import type { Day } from "./types.ts";

const W = 132;
const H = 32;
const PAD = 5;

/** Savings per day as a thin line. Each day has a hover title; the whole thing has a text summary. */
export function Sparkline({ days }: { days: Day[] }) {
  const max = Math.max(...days.map((d) => d.saved_vs_github_usd), 0.01);
  const step = (W - PAD * 2) / Math.max(days.length - 1, 1);
  const point = (d: Day, i: number) => [PAD + i * step, H - PAD - (d.saved_vs_github_usd / max) * (H - PAD * 2)] as const;
  const points = days.map(point);
  const total = days.reduce((n, d) => n + d.saved_vs_github_usd, 0);
  const [lx, ly] = points[points.length - 1] ?? [0, 0];
  return (
    <svg class="spark" viewBox={`0 0 ${W} ${H}`} role="img" aria-label={`Saved ${usd(total)} over the last ${days.length} days`}>
      <line class="spark-base" x1={PAD} x2={W - PAD} y1={H - PAD} y2={H - PAD} />
      <polyline class="spark-line" points={points.map((p) => p.join(",")).join(" ")} />
      {points.length > 0 && <circle class="spark-end" cx={lx} cy={ly} r="4" />}
      {days.map((d, i) => (
        <rect x={points[i][0] - step / 2} y="0" width={step} height={H} fill="transparent">
          <title>{`${shortDay(d.day)}: ${usd(d.saved_vs_github_usd)} saved, ${plural(d.jobs, "job")}`}</title>
        </rect>
      ))}
    </svg>
  );
}
