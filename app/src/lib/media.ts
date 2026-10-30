// The Tools (convert, make smaller, ZIP): types mirroring prism_video::media / prism_core::media,
// the size presets, which formats a file can become, and texts for estimates and problems.

import type { IconName } from "../design/icons/names";
import { fileExtension, formatBytes, formatClock } from "./format";
import { t, tk } from "./i18n";

export type MediaKind = "image" | "audio" | "video" | "other";
export type MediaTarget = "mp3" | "m4a" | "wav" | "flac" | "ogg" | "jpg" | "png" | "webp" | "mp4" | "webm" | "gif";
export type Quality = "standard" | "high";
export type MediaProblem =
  | "unreadable"
  | "heic"
  | "noAudio"
  | "tooLong"
  | "alreadySmall"
  | "overTarget"
  | "noSpace"
  | "access"
  | "missing"
  | "encoder"
  | "converter"
  | "failed";

/** One file as the Tools see it (`prism_video::media::MediaInfo`); echo it back to mediaPlan. */
export interface MediaFile {
  path: string;
  name: string;
  size: number;
  kind: MediaKind;
  ext: string;
  width?: number | null;
  height?: number | null;
  duration?: number | null;
  fps?: number | null;
  videoCodec?: string | null;
  audioCodec?: string | null;
  videoKbps?: number | null;
  audioKbps?: number | null;
  hasAudio: boolean;
  hasAlpha: boolean;
  hasCover: boolean;
  pixFmt?: string | null;
  tiled: boolean;
  /** A folder (only for a ZIP). */
  folder?: boolean;
  targets: MediaTarget[];
  problem?: MediaProblem | null;
}

export type Grade = "low" | "ok" | "good";

export interface ShrinkPlan {
  verdict: "ok" | "alreadySmall" | "tooLong" | "unknown";
  grade?: Grade | null;
  ext: string;
  estSize?: number | null;
  width?: number | null;
  height?: number | null;
  fps?: number | null;
  videoKbps?: number | null;
  audioKbps?: number | null;
}

export type MediaOp = { type: "convert"; target: MediaTarget; quality: Quality } | { type: "shrink"; targetBytes: number; label: string } | { type: "zip" };
export type JobState = "queued" | "preparing" | "running" | "done" | "failed" | "cancelled";

export interface MediaJob {
  id: string;
  op: MediaOp;
  inputs: string[];
  name: string;
  output?: string | null;
  state: JobState;
  progress?: number | null;
  retrying: boolean;
  problem?: MediaProblem | null;
  detail?: string | null;
  before: number;
  after?: number | null;
  createdAt: number;
  startedAt?: number | null;
  finishedAt?: number | null;
}

export type MediaRequest =
  | { op: "convert"; paths: string[]; target: MediaTarget; quality?: Quality }
  | { op: "shrink"; paths: string[]; targetBytes: number; label: string }
  | { op: "zip"; paths: string[] };

export type ToolsTab = "convert" | "shrink";
export type PresetId = "discord" | "email" | "whatsapp" | "custom";

/** What the Tools window is opened for (sidebar, row menu, completion window). */
export interface ToolsRequest {
  tab: ToolsTab;
  paths?: string[];
  preset?: PresetId;
}

export const MB = 1_000_000;

/** Upload limits (October 2026): Discord's free uploads went from 10 to 20 MB in August 2026. */
export const PRESETS: { id: Exclude<PresetId, "custom">; mb: number }[] = [
  { id: "discord", mb: 20 },
  { id: "email", mb: 25 },
  { id: "whatsapp", mb: 16 },
];

export const presetName = (id: PresetId) => (id === "discord" ? "Discord" : id === "whatsapp" ? "WhatsApp" : id === "email" ? t("E-mail") : t("Custom size"));

/** "Discord · 20 MB" */
export const presetLabel = (id: Exclude<PresetId, "custom">) => `${presetName(id)} · ${PRESETS.find((p) => p.id === id)?.mb ?? 0} MB`;

/** The word in the result's name: `clip (Discord).mp4`, `photo (smaller).jpg`. */
export const presetFileLabel = (id: PresetId) => (id === "custom" ? t("smaller") : presetName(id));

const IMAGE = ["jpg", "jpeg", "jfif", "png", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff", "gif"];
const AUDIO = ["mp3", "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "wma", "aif", "aiff", "alac", "ape", "wv", "m4b", "amr", "ac3"];
const VIDEO = ["mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "ts", "mts", "m2ts", "3gp", "ogv", "vob", "f4v"];

/** The kind a file's name suggests (menus decide without reading the file). */
export function kindByName(name: string): MediaKind {
  const ext = fileExtension(name);
  if (IMAGE.includes(ext)) return "image";
  if (AUDIO.includes(ext)) return "audio";
  if (VIDEO.includes(ext)) return "video";
  return "other";
}

const OWN: Record<string, MediaTarget> = { mp3: "mp3", m4a: "m4a", m4b: "m4a", wav: "wav", flac: "flac", ogg: "ogg", oga: "ogg", jpg: "jpg", jpeg: "jpg", jfif: "jpg", png: "png", webp: "webp", mp4: "mp4", m4v: "mp4", webm: "webm", gif: "gif" };

/** The formats offered in the row menu, most useful first (3–4, the file's own left out). */
export function quickTargets(name: string): MediaTarget[] {
  const own = OWN[fileExtension(name)];
  const kind = kindByName(name);
  const list: MediaTarget[] = kind === "image" ? ["jpg", "png", "webp"] : kind === "audio" ? ["mp3", "m4a", "wav", "flac"] : kind === "video" ? ["mp4", "mp3", "webm", "gif"] : [];
  // A moving GIF: a video for the tools, but its menu shows video formats.
  return list.filter((x) => x !== own).slice(0, 4);
}

export const TARGET_NAMES: Record<MediaTarget, string> = { mp3: "MP3", m4a: "M4A", wav: "WAV", flac: "FLAC", ogg: "OGG", jpg: "JPG", png: "PNG", webp: "WebP", mp4: "MP4", webm: "WebM", gif: "GIF" };

/** A short line under a format: what it's good for (no codec names). */
export function targetHint(target: MediaTarget, from: MediaKind): string {
  switch (target) {
    case "mp3":
      return from === "video" ? t("Sound only") : t("Plays everywhere");
    case "m4a":
      return t("Good for Apple devices");
    case "wav":
      return t("Uncompressed");
    case "flac":
      return t("Lossless, smaller than WAV");
    case "ogg":
      return t("Open format");
    case "jpg":
      return t("Opens everywhere");
    case "png":
      return t("Lossless, keeps transparency");
    case "webp":
      return t("Small, for the web");
    case "mp4":
      return t("Plays everywhere");
    case "webm":
      return t("For the web");
    case "gif":
      return t("Short clips, no sound");
  }
}

/** Lossy formats, where Standard / High quality makes a difference. */
export const LOSSY: MediaTarget[] = ["mp3", "m4a", "ogg", "jpg", "webp", "mp4", "webm", "gif"];

export const GIF_MAX_SECONDS = 60;

export function kindIcon(kind: MediaKind): IconName {
  return kind === "image" ? "file-image" : kind === "audio" ? "file-audio" : kind === "video" ? "file-video" : "file";
}

export function kindTint(kind: MediaKind): string {
  return kind === "image" ? "--type-image" : kind === "audio" ? "--type-audio" : kind === "video" ? "--type-video" : "--type-other";
}

const KIND_NAMES: Record<MediaKind, string> = { image: tk("Picture"), audio: tk("Sound"), video: tk("Video"), other: tk("File") };

/** "1080p" for a video (the short side), "4032 × 3024" for a picture. */
export function resolution(kind: MediaKind, w?: number | null, h?: number | null): string {
  if (!w || !h) return "";
  return kind === "video" ? `${Math.min(w, h)}p` : `${w} × ${h}`;
}

/** "Video · 1080p · 9:56" */
export function describe(f: MediaFile): string {
  if (f.folder) return t("Folder");
  const parts = [t(KIND_NAMES[f.kind])];
  const res = resolution(f.kind, f.width, f.height);
  if (res) parts.push(res);
  if (f.duration) parts.push(formatClock(f.duration));
  return parts.join(" · ");
}

/** What the converter couldn't do, in plain words. */
export function problemText(p: MediaProblem, ctx?: { size?: number; target?: MediaTarget }): string {
  switch (p) {
    case "unreadable":
      return t("The converter can’t read this file.");
    case "heic":
      return t("This HEIC photo can’t be read by the converter.");
    case "noAudio":
      return t("This video has no sound.");
    case "tooLong":
      return ctx?.target === "gif" ? t("GIF is for short clips: this video is longer than a minute.") : t("Too long for this size: it would be unwatchable. Pick a bigger size.");
    case "alreadySmall":
      return ctx?.size ? t("Already under {size}: nothing to do.", { size: `${Math.round(ctx.size / MB)} MB` }) : t("Already small enough: nothing to do.");
    case "overTarget":
      return t("Couldn’t get it under the size. Try a bigger size.");
    case "noSpace":
      return t("The disk is full.");
    case "access":
      return t("Prism can’t save next to the original (no permission).");
    case "missing":
      return t("The file isn’t there any more.");
    case "encoder":
      return t("This converter can’t make this format.");
    case "converter":
      return t("The converter (FFmpeg) couldn’t be downloaded. Check the connection and try again.");
    case "failed":
      return t("The conversion didn’t work.");
  }
}

export const GRADE_NAMES: Record<Grade, string> = { good: tk("Good"), ok: tk("OK"), low: tk("Low") };

/** "1080p → 720p · 30 fps · sound 128 kbps" (what changes, for one file). */
export function planSummary(f: MediaFile, p: ShrinkPlan): string {
  const parts: string[] = [];
  if (f.kind === "video" || f.kind === "image") {
    const from = resolution(f.kind, f.width, f.height);
    const to = resolution(f.kind, p.width, p.height);
    if (from && to && from !== to) parts.push(`${from} → ${to}`);
    else if (from) parts.push(from);
  }
  if (f.kind === "video" && p.fps && f.fps && p.fps + 0.5 < f.fps) parts.push(t("{fps} fps", { fps: Math.round(p.fps) }));
  if (p.audioKbps) parts.push(t("sound {kbps} kbps", { kbps: p.audioKbps }));
  if (f.kind === "image" && p.ext !== f.ext) parts.push(p.ext.toUpperCase());
  return parts.join(" · ");
}

/** A warning in plain words when the result won't be good, else "". */
export function planWarning(f: MediaFile, p: ShrinkPlan, targetBytes: number): string {
  const size = `${Math.round(targetBytes / MB)} MB`;
  const duration = formatClock(f.duration);
  if (p.verdict === "tooLong") {
    return f.kind === "image"
      ? t("This picture can’t get that small without turning into a thumbnail.")
      : f.kind === "audio"
        ? t("{duration} of sound can’t fit into {size}. Pick a bigger size.", { duration, size })
        : t("{duration} of video won’t fit into {size} and still be watchable. Pick a bigger size.", { duration, size });
  }
  if (p.verdict === "unknown") return t("The converter can’t read this file.");
  if (p.grade !== "low") return "";
  if (f.kind === "video") return t("{duration} of video is a lot for {size}: expect a blurry picture.", { duration, size });
  if (f.kind === "audio") return t("{duration} is a lot for {size}: the sound will be thin.", { duration, size });
  return t("The picture gets quite small.");
}

/** "412 MB → 19.2 MB" */
export const beforeAfter = (before: number, after?: number | null) => (after == null ? formatBytes(before) : `${formatBytes(before)} → ${formatBytes(after)}`);

/** The file name of a path (either separator). */
export const baseName = (path: string) => path.split(/[\\/]/).pop() ?? path;
