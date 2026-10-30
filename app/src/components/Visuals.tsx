// Small data visuals: progress bar and ring, file tile/thumbnail, speed chart, sparkline,
// connection lanes and file map.

import { memo, useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Icon } from "../design/icons";
import { formatBytes, formatClock, formatSpeed } from "../lib/format";
import { t } from "../lib/i18n";
import { fileVisual, type ProgressState } from "../lib/present";
import { smoothPath } from "../lib/ring";
import type { DownloadItem, SegmentInfo } from "../lib/types";

export function ProgressBar({ value, state, size, label }: { value: number | null; state: ProgressState; size?: "thin" | "thick" | "xl"; label?: string }) {
  const indeterminate = value == null && state === "active";
  return (
    <div
      className={`progress${size ? ` progress--${size}` : ""}`}
      data-state={indeterminate ? "indeterminate" : state}
      style={{ ["--value" as string]: String(value ?? 0) }}
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={value == null ? undefined : Math.round(value * 100)}
    >
      <div className="progress__fill" />
    </div>
  );
}

const RING_R = 44;
const RING_C = 2 * Math.PI * RING_R;

/** Circular progress with the same states as ProgressBar; children sit in the middle and
 *  give way to a check mark once complete. */
export function ProgressRing({ value, state, size = 96, label, children }: { value: number | null; state: ProgressState; size?: number; label?: string; children?: ReactNode }) {
  const id = useId().replace(/:/g, "");
  const indeterminate = value == null && state === "active";
  const v = Math.min(1, Math.max(0, value ?? 0));
  const offset = RING_C * (1 - (indeterminate ? 0.25 : v));
  // Celebrate only a finish seen live, not opening an already-finished download. A layout
  // effect so the burst starts before paint (no flash of a fully drawn check first).
  const prev = useRef(state);
  const [burst, setBurst] = useState(false);
  useLayoutEffect(() => {
    if (state === "completed" && prev.current !== "completed") setBurst(true);
    if (state !== "completed") setBurst(false);
    prev.current = state;
  }, [state]);
  return (
    <div
      className={`ring${burst ? " ring--burst" : ""}`}
      data-state={indeterminate ? "indeterminate" : state}
      style={{ width: size, height: size }}
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={state === "completed" ? 100 : value == null ? undefined : Math.round(v * 100)}
    >
      <svg viewBox="0 0 100 100" aria-hidden="true">
        <defs>
          <linearGradient id={`rg${id}`} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" className="ring__stop-a" />
            <stop offset="100%" className="ring__stop-b" />
          </linearGradient>
          {/* The sweep is masked by the filled arc so it never runs over the empty track. */}
          <mask id={`rm${id}`}>
            <circle cx="50" cy="50" r={RING_R} className="ring__arc" stroke="#fff" strokeDasharray={RING_C} strokeDashoffset={offset} transform="rotate(-90 50 50)" />
          </mask>
        </defs>
        <circle cx="50" cy="50" r={RING_R} className="ring__track" />
        <g className="ring__spin">
          <circle cx="50" cy="50" r={RING_R} className="ring__arc ring__fill" stroke={`url(#rg${id})`} strokeDasharray={RING_C} strokeDashoffset={offset} transform="rotate(-90 50 50)" />
          <g mask={`url(#rm${id})`}>
            <circle cx="50" cy="50" r={RING_R} className="ring__arc ring__sweep" strokeDasharray={`${RING_C * 0.16} ${RING_C}`} />
          </g>
        </g>
        <g className="ring__head" style={{ transform: `rotate(${v * 360}deg)` }}>
          <circle cx="50" cy={50 - RING_R} r="7" className="ring__head-glow" />
          <circle cx="50" cy={50 - RING_R} r="4" className="ring__head-dot" />
        </g>
        <circle cx="50" cy="50" r={RING_R} className="ring__ripple" />
        <circle cx="50" cy="50" r={RING_R} className="ring__ripple" />
      </svg>
      <div className="ring__label">
        {state === "completed" ? (
          <svg className="ring__check" width="34" height="34" viewBox="0 0 34 34" aria-hidden="true">
            <path d="M9 17.5l5.5 5.5L25 12.5" />
          </svg>
        ) : (
          children
        )}
      </div>
    </div>
  );
}

/** A number whose digits roll like an odometer; other characters are shown as is. */
export function RollingNumber({ text }: { text: string }) {
  return (
    <span className="roll">
      {[...text].map((ch, i) =>
        /\d/.test(ch) ? (
          // keyed from the right so units stay put when the number grows a digit
          <span key={text.length - i} className="roll__col" aria-hidden="true" style={{ transform: `translateY(${-Number(ch)}em)` }}>
            {"0123456789".split("").map((d) => (
              <span key={d}>{d}</span>
            ))}
          </span>
        ) : (
          <span key={`c${text.length - i}`}>{ch}</span>
        ),
      )}
    </span>
  );
}

/** 28 px type tile, or a 44×28 thumbnail for media with artwork. */
export const FileTile = memo(function FileTile({ item, size = 28, thumbWidth = 44 }: { item: Pick<DownloadItem, "fileName" | "kind" | "category" | "media">; size?: number; thumbWidth?: number }) {
  const thumb = item.media?.thumbnail;
  if (thumb) {
    return (
      <span className="thumb" style={{ width: thumbWidth, height: size, backgroundImage: `url("${thumb}")` }}>
        {item.media?.duration ? <span className="thumb__dur">{formatClock(item.media.duration)}</span> : null}
      </span>
    );
  }
  const v = fileVisual(item);
  return (
    <span className="file-tile" style={{ ["--tint" as string]: `var(${v.tint})`, width: size, height: size, borderRadius: Math.round(size * 0.29) }}>
      <Icon name={v.icon} size={Math.round(size * 0.58)} />
    </span>
  );
});

/** Area chart for speed history. Horizontal gridlines with labels in MB/s. */
export function SpeedChart({ samples, height = 160, labels = true }: { samples: number[]; height?: number; labels?: boolean }) {
  const id = useId().replace(/:/g, "");
  const W = 300;
  const H = 100;
  const max = niceMax(Math.max(...samples, 1));
  const { line, area } = smoothPath(samples.length > 1 ? samples : [0, 0], W, H, max / 1.1, 3);
  const last = samples[samples.length - 1] ?? 0;
  const lastY = H - 3 - (last / max) * (H - 6);
  const ticks = [0.25, 0.5, 0.75, 1];
  return (
    <div className="chart" style={{ height }}>
      {labels &&
        ticks.map((tick) => (
          <span key={tick} className="chart__ylab" style={{ top: `${(1 - tick) * 100}%` }}>
            {formatBytes(max * tick, 0).replace(" ", " ")}
          </span>
        ))}
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden="true">
        <defs>
          <linearGradient id={`${id}a`} x1="0" x2="0" y1="0" y2="1">
            <stop offset="0" stopColor="var(--accent)" stopOpacity=".38" />
            <stop offset="1" stopColor="var(--accent)" stopOpacity="0" />
          </linearGradient>
        </defs>
        {ticks.map((tick) => (
          <path key={tick} d={`M0 ${H - tick * H}H${W}`} stroke="var(--divider)" vectorEffect="non-scaling-stroke" />
        ))}
        <path d={area} fill={`url(#${id}a)`} />
        <path d={line} fill="none" stroke="var(--accent)" strokeWidth="1.75" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
      </svg>
      {samples.length > 1 && <span className="chart__dot" style={{ top: `${(lastY / H) * 100}%` }} />}
      <span className="sr-only">{t("Current speed {speed}", { speed: formatSpeed(last) })}</span>
    </div>
  );
}

function niceMax(v: number): number {
  const mb = 1024 * 1024;
  const steps = [256 * 1024, 512 * 1024, mb, 2 * mb, 4 * mb, 5 * mb, 10 * mb, 20 * mb, 25 * mb, 40 * mb, 50 * mb, 80 * mb, 100 * mb, 200 * mb, 400 * mb, 1000 * mb];
  return steps.find((s) => s >= v * 1.05) ?? v * 1.2;
}

export function Sparkline({ samples, width = 196, height = 38 }: { samples: number[]; width?: number; height?: number }) {
  const id = useId().replace(/:/g, "");
  const { line, area } = smoothPath(samples.length > 1 ? samples : [0, 0], width, height, undefined, 2);
  return (
    <svg className="spark" viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" aria-hidden="true">
      <defs>
        <linearGradient id={`${id}s`} x1="0" x2="0" y1="0" y2="1">
          <stop offset="0" stopColor="var(--accent)" stopOpacity=".35" />
          <stop offset="1" stopColor="var(--accent)" stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={area} fill={`url(#${id}s)`} />
      <path d={line} stroke="var(--accent)" strokeWidth="1.5" fill="none" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

const spectrum = (i: number) => `var(--spectrum-${(i % 8) + 1})`;

/** Whole-file map: each segment occupies its range (bytes, or — for HLS/DASH — a position
 *  among the media segments); the filled part is coloured per connection. */
export function FileMap({ segments, size, unit = "bytes" }: { segments: SegmentInfo[]; size: number; unit?: "bytes" | "segments" }) {
  const total = unit === "segments" ? segments.length : size;
  if (!total || !segments.length) return <div className="segbar" />;
  return (
    <div className="segbar" role="img" aria-label={t("{count} segments", { count: segments.length })}>
      {segments.map((s, i) => {
        const span = Math.max(1, s.end - s.start);
        return (
          <span
            key={i}
            className="segbar__seg"
            data-active={s.state === "active" && s.pos < s.end}
            style={{
              ["--a" as string]: s.start / total,
              ["--b" as string]: s.end / total,
              ["--p" as string]: Math.min(1, (s.pos - s.start) / span),
              ["--c" as string]: spectrum(i),
            }}
          />
        );
      })}
    </div>
  );
}

/** One lane per connection — the segments actually being transferred right now, never the
 *  whole (for HLS/DASH, hundreds-strong) segment list. */
export function ConnectionLanes({ segments, max, unit = "bytes" }: { segments: SegmentInfo[]; max?: number; unit?: "bytes" | "segments" }) {
  const active = segments.filter((s) => s.state === "active");
  const shown = max ? active.slice(0, max) : active;
  if (!active.length) return <p className="lanes__empty">{t("No active connections.")}</p>;
  return (
    <div className="lanes">
      {shown.map((s, i) => {
        const span = Math.max(1, s.end - s.start);
        const f = Math.min(1, (s.pos - s.start) / span);
        return (
          <div key={i} className="lane" style={{ ["--c" as string]: spectrum(i) }}>
            <span className="lane__id">{String(i + 1).padStart(2, "0")}</span>
            <span className="lane__track">
              <span className="lane__fill" style={{ transform: `scaleX(${f})` }} />
              <span className="lane__head" style={{ left: `calc(${f * 100}% - 1px)` }} />
            </span>
            <span className="lane__range">{unit === "segments" ? t("Segment {number}", { number: s.start + 1 }) : `${formatBytes(s.start)}–${formatBytes(s.end)}`}</span>
            {/* The engine measures speed per connection, not per media segment. */}
            <span className="lane__speed">{unit === "segments" ? "" : formatSpeed(s.speed)}</span>
          </div>
        );
      })}
      {max && active.length > max && <p className="lanes__more">{t("+ {count} more connections", { count: active.length - max })}</p>}
    </div>
  );
}
