// A part of a video ("from 1:23 to 2:45"): typed times, the range "Only a part" starts with,
// chapter shortcuts and how a part reads in the window, the list and the details.

import { formatClock } from "./format";
import type { VideoSection } from "./types";

/** A chapter of the video (`prism_video::Chapter`). */
export interface VideoChapter {
  start: number;
  end: number;
  title: string;
}

/** The shortest part, in seconds (the core cuts nothing shorter). */
export const MIN_PART = 1;

/** Whole seconds from a typed time: `83`, `1:23`, `01:23`, `1:02:03` (a fraction is rounded). */
export function parseTime(text: string): number | null {
  const s = text.trim();
  if (!/^\d+(?::\d{1,2}){0,2}(?:\.\d+)?$/.test(s)) return null;
  const parts = s.split(":").map(Number);
  if (parts.slice(1).some((p) => p >= 60)) return null;
  return Math.round(parts.reduce((total, p) => total * 60 + p, 0));
}

/** `1:23–2:45` */
export const formatRange = (r: VideoSection): string => `${formatClock(r.start)}–${formatClock(r.end)}`;

/** Kept left to right inside any text, so a range doesn't flip in Arabic. */
export const ltr = (s: string): string => `⁦${s}⁩`;

/** The timeline's length in whole seconds. */
export const timelineLength = (duration: number): number => Math.max(MIN_PART, Math.round(duration));

/** A range on a timeline `total` seconds long: whole seconds, in order, at least `MIN_PART` long. */
export function clampRange(start: number, end: number, total: number): VideoSection {
  const s = Math.min(Math.max(0, Math.round(start)), total - MIN_PART);
  const e = Math.max(Math.min(total, Math.round(end)), s + MIN_PART);
  return { start: s, end: e };
}

/** A new start typed in: past the end, the part moves along (keeping its length). */
export function withStart(r: VideoSection, start: number, total: number): VideoSection {
  return start < r.end ? clampRange(start, r.end, total) : clampRange(start, start + (r.end - r.start), total);
}

/** A new end typed in: before the start, the part moves back (keeping its length). */
export function withEnd(r: VideoSection, end: number, total: number): VideoSection {
  return end > r.start ? clampRange(r.start, end, total) : clampRange(end - (r.end - r.start), end, total);
}

/** The range "Only a part" starts with: the first chapter, else the first minute (all of a shorter video). */
export function defaultRange(total: number, chapters: VideoChapter[]): VideoSection {
  const first = chapters[0];
  return first ? clampRange(first.start, first.end, total) : clampRange(0, Math.min(60, total), total);
}

/** The range covers the whole video: that's a normal download. */
export const isWhole = (r: VideoSection, total: number): boolean => r.start <= 0 && r.end >= total;

export const sameRange = (a: VideoSection, b: VideoSection): boolean => a.start === b.start && a.end === b.end;

/** A part's estimated size: the format's size × part / whole. */
export function partSize(size: number | null | undefined, r: VideoSection, duration: number): number | null {
  return size && duration > 0 ? Math.round((size * (r.end - r.start)) / duration) : null;
}

/** yt-dlp names chapters without a title `<Untitled Chapter 2>`: their number, else null. */
export function untitledChapter(title: string): number | null {
  const m = /^<Untitled Chapter (\d+)>$/.exec(title.trim());
  return m ? Number(m[1]) : null;
}

/** The value a timeline handle gets from a key (arrows: 1 s, Shift or Page keys: 10 s), or null. */
export function keyStep(key: string, shift: boolean, value: number, min: number, max: number): number | null {
  const step = shift ? 10 : 1;
  switch (key) {
    case "ArrowLeft":
    case "ArrowDown":
      return Math.max(min, value - step);
    case "ArrowRight":
    case "ArrowUp":
      return Math.min(max, value + step);
    case "PageDown":
      return Math.max(min, value - 10);
    case "PageUp":
      return Math.min(max, value + 10);
    case "Home":
      return min;
    case "End":
      return max;
    default:
      return null;
  }
}
