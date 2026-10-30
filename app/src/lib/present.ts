// Pure presentation helpers: how an item's state, type and numbers are shown.

import type { IconName } from "../design/icons/names";
import { fileExtension, formatBytes, hostOf, percent } from "./format";
import { t, tk } from "./i18n";
import type { Category, DownloadItem, Progress, Status } from "./types";
import { formatRange, ltr } from "./videoPart";

export type Tone = "accent" | "success" | "warning" | "danger" | "info" | "neutral";
export type ProgressState = "active" | "paused" | "queued" | "completed" | "error";

export interface StatusView {
  label: string;
  /** Longer text for tooltips / the status column (error message, queue position…). */
  detail: string;
  tone: Tone;
  icon: IconName;
  bar: ProgressState;
}

const STATUS: Record<Status, Omit<StatusView, "detail">> = {
  idle: { label: tk("Stopped"), tone: "neutral", icon: "status-paused", bar: "queued" },
  queued: { label: tk("Queued"), tone: "neutral", icon: "status-queued", bar: "queued" },
  connecting: { label: tk("Connecting"), tone: "accent", icon: "status-waiting", bar: "active" },
  downloading: { label: tk("Downloading"), tone: "accent", icon: "status-downloading", bar: "active" },
  retrying: { label: tk("Retrying"), tone: "warning", icon: "status-waiting", bar: "paused" },
  paused: { label: tk("Paused"), tone: "warning", icon: "status-paused", bar: "paused" },
  processing: { label: tk("Processing"), tone: "info", icon: "status-merging", bar: "active" },
  completed: { label: tk("Completed"), tone: "success", icon: "status-completed", bar: "completed" },
  failed: { label: tk("Failed"), tone: "danger", icon: "status-error", bar: "error" },
};

export function statusView(item: DownloadItem, progress?: Progress | null): StatusView {
  const status = progress?.status ?? item.status;
  const base = STATUS[status] ?? STATUS.idle;
  const label = t(base.label);
  let detail = label;
  if (status === "failed" && item.error) detail = item.error;
  else if (status === "processing" && progress?.detail) detail = progress.detail;
  else if (status === "retrying" && progress?.detail) detail = progress.detail;
  else if (status === "connecting") detail = t("Connecting…");
  return { ...base, label, detail };
}

/** Title-row status filter, applied on top of the sidebar view. */
export function matchesFilter(status: Status, filter: "all" | "active" | "queued" | "completed" | "failed"): boolean {
  switch (filter) {
    case "all":
      return true;
    case "active":
      return ["connecting", "downloading", "retrying", "processing"].includes(status);
    case "queued":
      return status === "queued" || status === "idle" || status === "paused";
    case "completed":
      return status === "completed";
    case "failed":
      return status === "failed";
  }
}

export const isUnfinished = (s: Status) => s !== "completed";
export const canPause = (s: Status) => ["queued", "connecting", "downloading", "retrying", "processing"].includes(s);
export const canResume = (s: Status) => ["idle", "paused", "failed"].includes(s);

/** Done bytes, total and 0..1 fraction (null when the size is unknown). A part of a video being
 *  cut has no size until it's done; its share done comes with the progress instead. */
export function progressOf(item: DownloadItem, progress?: Progress | null) {
  const done = progress?.downloaded ?? item.downloaded;
  const share = item.status === "completed" ? null : progress?.fraction;
  const total = share != null ? null : (progress?.size ?? item.size ?? null);
  const pct = item.status === "completed" ? 100 : share != null ? Math.min(100, Math.max(0, share * 100)) : percent(done, total);
  return { done, total, fraction: pct == null ? null : pct / 100, pct };
}

export function formatPct(pct: number | null): string {
  if (pct == null) return "—";
  if (pct >= 100) return "100%";
  return `${pct.toFixed(1)}%`;
}

/** "3.64 GB / 5.83 GB", or just the total once complete. */
export function bytesLine(item: DownloadItem, progress?: Progress | null): string {
  const { done, total } = progressOf(item, progress);
  if (item.status === "completed") return formatBytes(total ?? done);
  if (!total) return done ? formatBytes(done) : "";
  return `${formatBytes(done)} / ${formatBytes(total)}`;
}

// ---- file types -------------------------------------------------------------------

const EXT: Record<string, { icon: IconName; tint: string }> = {};
const add = (exts: string, icon: IconName, tint: string) => exts.split(" ").forEach((e) => (EXT[e] = { icon, tint }));
add("mp4 mkv webm mov avi m4v flv wmv ts m3u8 mpd", "file-video", "--type-video");
add("mp3 m4a flac wav ogg opus aac wma", "file-audio", "--type-audio");
add("jpg jpeg png gif webp avif bmp svg tif tiff heic raw", "file-image", "--type-image");
add("zip rar 7z tar gz xz zst bz2 tgz cab", "file-archive", "--type-archive");
add("pdf", "file-pdf", "--type-document");
add("doc docx txt md rtf odt xls xlsx csv ppt pptx epub", "file-document", "--type-document");
add("exe msi msix appx dmg pkg deb rpm appimage apk", "file-exe", "--type-program");
add("iso img vhd vhdx", "disk-image", "--type-program");
add("ttf otf woff woff2", "file-font", "--type-other");
add("js ts py rs go c cpp java json xml html css sh", "file-code", "--type-other");
add("torrent", "torrent", "--type-other");

export function fileVisual(item: Pick<DownloadItem, "fileName" | "kind" | "category">): { icon: IconName; tint: string } {
  if (item.kind === "torrent") return { icon: "magnet", tint: "--type-archive" };
  const byExt = EXT[fileExtension(item.fileName)];
  if (byExt) return byExt;
  if (item.kind === "video" || item.kind === "hls" || item.kind === "dash" || item.category === "video")
    return { icon: "file-video", tint: "--type-video" };
  if (item.category === "music") return { icon: "file-audio", tint: "--type-audio" };
  return { icon: "file", tint: "--type-other" };
}

const CATEGORY_ICONS: Record<string, IconName> = {
  video: "video",
  music: "music",
  documents: "documents",
  programs: "programs",
  compressed: "compressed",
  images: "images",
  other: "other",
};

// Built-in categories and queues come from the core with English names: show them translated
// while they keep that name (a renamed one shows as typed). Folders on disk keep the real name.
const BUILTIN_NAMES = new Set([
  tk("Video"),
  tk("Music"),
  tk("Documents"),
  tk("Programs"),
  tk("Compressed"),
  tk("Images"),
  tk("Other"),
  tk("Main queue"),
  tk("One at a time"),
]);

/** A category's or queue's name as shown in the UI. */
export const displayName = (name: string): string => (BUILTIN_NAMES.has(name) ? t(name) : name);

/** The category of a set of files (a torrent): the one its largest file belongs to, else "other". */
export function categoryForFiles(files: { path: string; size: number }[], categories: Pick<Category, "id" | "extensions">[]): string {
  const biggest = files.reduce<{ path: string; size: number } | null>((a, f) => (!a || f.size > a.size ? f : a), null);
  const ext = biggest ? fileExtension(biggest.path) : "";
  const match = ext ? categories.find((c) => c.extensions.some((e) => e.replace(/^\./, "").toLowerCase() === ext)) : undefined;
  return match?.id ?? "other";
}

export function categoryIcon(c: Pick<Category, "id" | "builtin">): IconName {
  return CATEGORY_ICONS[c.id] ?? "custom-folder";
}

/** "host · category" meta line under a file name. */
export function metaLine(item: DownloadItem, categories: Category[]): string {
  const host = item.media?.site || hostOf(item.finalUrl || item.url) || (item.url.startsWith("magnet:") ? "BitTorrent" : "");
  const quality = item.media?.quality;
  const cat = displayName(categories.find((c) => c.id === item.category)?.name ?? "");
  const part = item.media?.section ? t("Part {range}", { range: ltr(formatRange(item.media.section)) }) : null;
  return [host, quality || cat, part].filter(Boolean).join(" · ");
}

// ---- input helpers ------------------------------------------------------------------

const URL_RE = /^(https?|ftps?):\/\/[^\s]+$/i;

export function looksLikeUrl(text: string): boolean {
  const s = text.trim();
  return URL_RE.test(s) || isTorrentLink(s);
}

export function isTorrentLink(url: string): boolean {
  const u = url.trim().toLowerCase();
  return u.startsWith("magnet:?") || /\.torrent(?:[?#].*)?$/.test(u);
}

/** Extract URLs from dropped/pasted text (uri-list or plain text). */
export function extractUrls(text: string): string[] {
  const out: string[] = [];
  for (const raw of text.split(/[\s\r\n]+/)) {
    const s = raw.trim();
    if (!s || s.startsWith("#")) continue;
    if (looksLikeUrl(s) && !out.includes(s)) out.push(s);
  }
  return out;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) bin += String.fromCharCode(...bytes.subarray(i, i + chunk));
  return btoa(bin);
}

/** Middle-truncate a file name keeping its extension. */
export function middleTruncate(name: string, max = 48): string {
  if (name.length <= max) return name;
  const ext = fileExtension(name);
  const tail = ext ? ext.length + 6 : 8;
  return `${name.slice(0, max - tail - 1)}…${name.slice(name.length - tail)}`;
}
