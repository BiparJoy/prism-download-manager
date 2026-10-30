// "Only a part" of a video: a timeline with two handles (mouse, touch, keyboard), the start and
// end as typed times, the part's length and size, and the video's chapters as shortcuts.

import { type PointerEvent as ReactPointerEvent, useRef, useState } from "react";
import { formatBytes, formatClock } from "../lib/format";
import { t } from "../lib/i18n";
import type { VideoSection } from "../lib/types";
import { MIN_PART, type VideoChapter, clampRange, formatRange, keyStep, parseTime, sameRange, untitledChapter, withEnd, withStart } from "../lib/videoPart";

type Handle = "start" | "end";

export function PartPicker({
  total,
  chapters,
  value,
  onChange,
  size,
}: {
  /** The video's length in whole seconds. */
  total: number;
  chapters: VideoChapter[];
  value: VideoSection;
  onChange: (r: VideoSection) => void;
  /** Estimated size of the part, if known. */
  size: number | null;
}) {
  return (
    <div className="vpart" role="group" aria-label={t("Part of the video")}>
      <Timeline total={total} chapters={chapters} value={value} onChange={onChange} />
      <div className="vpart__times">
        <TimeField text={t("From")} label={t("Start of the part")} value={value.start} onCommit={(v) => onChange(withStart(value, v, total))} />
        <TimeField text={t("to")} label={t("End of the part")} value={value.end} onCommit={(v) => onChange(withEnd(value, v, total))} />
        <span className="vpart__sum">
          {t("Length")} <b className="tnum">{formatClock(value.end - value.start)}</b>
          {size ? (
            <>
              <span aria-hidden="true"> · </span>
              <b className="tnum">~{formatBytes(size)}</b>
            </>
          ) : null}
        </span>
      </div>
      {chapters.length > 0 && <Chapters chapters={chapters} total={total} value={value} onPick={onChange} />}
    </div>
  );
}

/** The video as a bar: the part highlighted between two handles, chapter starts as faint marks. */
function Timeline({ total, chapters, value, onChange }: { total: number; chapters: VideoChapter[]; value: VideoSection; onChange: (r: VideoSection) => void }) {
  const bar = useRef<HTMLDivElement>(null);
  const thumbs = { start: useRef<HTMLSpanElement>(null), end: useRef<HTMLSpanElement>(null) };
  const [dragging, setDragging] = useState<Handle | null>(null);
  // Both handles under the pointer (a very short part): the first move decides which one.
  const pending = useRef<number | null>(null);
  const pct = (s: number) => `${(s / total) * 100}%`;

  const at = (clientX: number) => {
    const box = bar.current?.getBoundingClientRect();
    if (!box || box.width <= 0) return 0;
    return Math.min(total, Math.max(0, Math.round(((clientX - box.left) / box.width) * total)));
  };
  const move = (h: Handle, v: number) => {
    const next = h === "start" ? clampRange(Math.min(v, value.end - MIN_PART), value.end, total) : clampRange(value.start, Math.max(v, value.start + MIN_PART), total);
    if (!sameRange(next, value)) onChange(next);
  };
  const grab = (h: Handle) => {
    setDragging(h);
    thumbs[h].current?.focus({ preventScroll: true });
  };

  const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    const v = at(e.clientX);
    const box = bar.current?.getBoundingClientRect();
    const px = box ? ((value.end - value.start) / total) * box.width : Infinity;
    const onThumb = (e.target as HTMLElement).dataset.thumb as Handle | undefined;
    if (onThumb && px < 12) {
      pending.current = e.clientX;
      return;
    }
    const h = onThumb ?? (Math.abs(v - value.start) <= Math.abs(v - value.end) ? "start" : "end");
    grab(h);
    if (!onThumb) move(h, v);
  };
  const onPointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    let h = dragging;
    if (!h && pending.current != null) {
      if (Math.abs(e.clientX - pending.current) < 2) return;
      h = e.clientX < pending.current ? "start" : "end";
      pending.current = null;
      grab(h);
    }
    if (h) move(h, at(e.clientX));
  };
  const release = () => {
    pending.current = null;
    setDragging(null);
  };

  const thumb = (h: Handle, label: string, min: number, max: number) => {
    const v = value[h];
    return (
      <span
        ref={thumbs[h]}
        className={`vtl__thumb${dragging === h ? " is-dragging" : ""}`}
        data-thumb={h}
        role="slider"
        tabIndex={0}
        aria-label={label}
        aria-orientation="horizontal"
        aria-valuemin={min}
        aria-valuemax={max}
        aria-valuenow={v}
        aria-valuetext={formatClock(v)}
        style={{ left: pct(v) }}
        onKeyDown={(e) => {
          const n = keyStep(e.key, e.shiftKey, v, min, max);
          if (n == null) return;
          e.preventDefault();
          move(h, n);
        }}
      />
    );
  };

  return (
    <div ref={bar} className={`vtl${dragging ? " is-dragging" : ""}`} onPointerDown={onPointerDown} onPointerMove={onPointerMove} onPointerUp={release} onPointerCancel={release}>
      <span className="vtl__track" />
      {chapters.slice(1).map((c, i) => (
        <span key={i} className="vtl__mark" style={{ left: pct(Math.min(total, c.start)) }} />
      ))}
      <span className="vtl__range" style={{ left: pct(value.start), width: pct(value.end - value.start) }} />
      {thumb("start", t("Start of the part"), 0, value.end - MIN_PART)}
      {thumb("end", t("End of the part"), value.start + MIN_PART, total)}
    </div>
  );
}

/** A time typed as `83`, `1:23` or `1:02:03` (Enter or leaving the field takes it, Escape undoes
 *  it); shown as `1:23` again once taken. */
function TimeField({ text, label, value, onCommit }: { text: string; label: string; value: number; onCommit: (v: number) => void }) {
  const [draft, setDraft] = useState<string | null>(null);
  const [invalid, setInvalid] = useState(false);
  const commit = () => {
    if (draft == null) return;
    const v = parseTime(draft);
    setInvalid(v == null);
    if (v == null) return;
    setDraft(null);
    onCommit(v);
  };
  return (
    <label className="vpart__field">
      <span aria-hidden="true">{text}</span>
      <span className={`input${invalid ? " input--invalid" : ""}`}>
        <input
          className="tnum"
          value={draft ?? formatClock(value)}
          aria-label={label}
          aria-invalid={invalid || undefined}
          spellCheck={false}
          autoComplete="off"
          onChange={(e) => {
            setDraft(e.target.value);
            setInvalid(false);
          }}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
            if (e.key === "Escape" && draft != null) {
              e.preventDefault();
              setDraft(null);
              setInvalid(false);
            }
          }}
        />
      </span>
    </label>
  );
}

/** The video's chapters: picking one selects it. */
function Chapters({ chapters, total, value, onPick }: { chapters: VideoChapter[]; total: number; value: VideoSection; onPick: (r: VideoSection) => void }) {
  return (
    <div className="vchips" role="group" aria-label={t("Chapters")}>
      {chapters.map((c, i) => {
        const r = clampRange(c.start, c.end, total);
        const n = untitledChapter(c.title) ?? (c.title.trim() ? null : i + 1);
        const name = n != null ? t("Chapter {number}", { number: n }) : c.title.trim();
        const on = sameRange(r, value);
        return (
          <button key={i} type="button" className={`vchip${on ? " on" : ""}`} aria-pressed={on} data-tip={`${name} · ${formatRange(r)}`} onClick={() => onPick(r)}>
            <span className="vchip__at tnum">{formatClock(r.start)}</span>
            <span className="vchip__name">{name}</span>
          </button>
        );
      })}
    </div>
  );
}
