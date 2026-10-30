import { describe, expect, it } from "vitest";
import { gridTemplate, resolveColumns, toggleColumn } from "./columns";
import { formatDay } from "./format";
import { bytesToBase64, categoryForFiles, extractUrls, fileVisual, isTorrentLink, looksLikeUrl, matchesFilter, middleTruncate, progressOf, statusView } from "./present";
import { Ring, smoothPath } from "./ring";
import { hexDecode, hexEncode, parseRoute } from "./route";
import { clickSelect, moveSelect } from "./selection";
import { buildTree, nodeState } from "./tree";
import type { DownloadItem } from "./types";

const item = (over: Partial<DownloadItem> = {}): DownloadItem => ({
  id: "a",
  kind: "http",
  url: "https://example.com/file.zip",
  fileName: "file.zip",
  saveDir: "C:\\Downloads",
  size: 1000,
  downloaded: 250,
  status: "downloading",
  category: "compressed",
  description: "",
  addedAt: 0,
  connections: 8,
  speedLimit: 0,
  trashed: false,
  position: 0,
  mirrors: [],
  ...over,
});

describe("route", () => {
  it("parses hash routes", () => {
    expect(parseRoute("")).toEqual({ name: "main" });
    expect(parseRoute("#/")).toEqual({ name: "main" });
    expect(parseRoute("#/capture/cap7")).toEqual({ name: "capture", id: "cap7" });
    expect(parseRoute("#/details/abc")).toEqual({ name: "details", id: "abc" });
    expect(parseRoute("#/mini")).toEqual({ name: "mini" });
    expect(parseRoute("#/capture")).toEqual({ name: "main" });
    expect(parseRoute("#/unknown/x")).toEqual({ name: "main" });
  });
  it("decodes Rust's hex-encoded torrent argument (UTF-8 safe)", () => {
    const src = "C:\\Users\\jöhn\\ubuntu.torrent";
    expect(hexDecode(hexEncode(src))).toBe(src);
    expect(parseRoute(`#/torrent/${hexEncode("magnet:?xt=urn:btih:abc")}`)).toEqual({ name: "torrent", source: "magnet:?xt=urn:btih:abc" });
    expect(hexDecode("not-hex")).toBe("not-hex");
  });
  it("opens the model window with a name or link, or empty to search", () => {
    expect(parseRoute(`#/model/${hexEncode("https://huggingface.co/Qwen/Qwen3-8B")}`)).toEqual({ name: "model", source: "https://huggingface.co/Qwen/Qwen3-8B" });
    expect(parseRoute("#/model")).toEqual({ name: "model", source: "" });
  });
});

describe("selection", () => {
  const order = ["a", "b", "c", "d", "e"];
  it("click / ctrl / shift", () => {
    let s = clickSelect(order, { selection: [], anchor: null }, "b");
    expect(s).toEqual({ selection: ["b"], anchor: "b" });
    s = clickSelect(order, s, "d", { shift: true });
    expect(s.selection).toEqual(["b", "c", "d"]);
    s = clickSelect(order, s, "c", { ctrl: true });
    expect(s.selection).toEqual(["b", "d"]);
    s = clickSelect(order, { selection: ["a"], anchor: "a" }, "e", { shift: true, ctrl: true });
    expect(s.selection).toEqual(["a", "b", "c", "d", "e"]);
  });
  it("keyboard moves and extends", () => {
    let s = moveSelect(order, { selection: [], anchor: null }, "down");
    expect(s.selection).toEqual(["a"]);
    s = moveSelect(order, s, "down", true);
    expect(s.selection).toEqual(["a", "b"]);
    s = moveSelect(order, s, "end");
    expect(s.selection).toEqual(["e"]);
    s = moveSelect(order, s, "pageUp", false, 3);
    expect(s.selection).toEqual(["b"]);
    expect(moveSelect([], s, "down")).toEqual({ selection: [], anchor: null });
  });
});

describe("columns", () => {
  it("keeps canonical order and always shows the name", () => {
    expect(resolveColumns(["speed", "size"]).map((c) => c.key)).toEqual(["name", "size", "speed"]);
    expect(toggleColumn(["name", "size"], "eta")).toEqual(["name", "size", "eta"]);
    expect(toggleColumn(["name", "size"], "size")).toEqual(["name"]);
    expect(toggleColumn(["name"], "name")).toEqual(["name"]);
  });
  it("builds the grid template with persisted widths and minimums", () => {
    const cols = resolveColumns(["name", "size", "speed"]);
    expect(gridTemplate(cols, { size: 40, speed: 120 })).toBe("36px minmax(160px, 1fr) 64px 120px 40px");
  });
});

describe("present", () => {
  it("status and progress", () => {
    expect(statusView(item({ status: "failed", error: "403 Forbidden" })).detail).toBe("403 Forbidden");
    expect(statusView(item(), { id: "a", status: "paused", downloaded: 1, speed: 0, connections: 0 }).label).toBe("Paused");
    expect(progressOf(item()).fraction).toBe(0.25);
    expect(progressOf(item({ size: null })).fraction).toBeNull();
    expect(progressOf(item({ status: "completed", size: null })).pct).toBe(100);
  });
  it("filters", () => {
    expect(matchesFilter("downloading", "active")).toBe(true);
    expect(matchesFilter("paused", "queued")).toBe(true);
    expect(matchesFilter("completed", "failed")).toBe(false);
  });
  it("file visuals", () => {
    expect(fileVisual(item({ fileName: "movie.MKV" })).icon).toBe("file-video");
    expect(fileVisual(item({ kind: "torrent" })).icon).toBe("magnet");
    expect(fileVisual(item({ fileName: "noext", category: "music" })).icon).toBe("file-audio");
  });
  it("url helpers", () => {
    expect(looksLikeUrl("https://a.b/c")).toBe(true);
    expect(looksLikeUrl("hello world")).toBe(false);
    expect(isTorrentLink("https://x.org/a.torrent?dl=1")).toBe(true);
    expect(isTorrentLink("magnet:?xt=urn:btih:1")).toBe(true);
    expect(extractUrls("# comment\nhttps://a.com/x.zip\r\nnot a url https://a.com/x.zip ftp://h/f")).toEqual(["https://a.com/x.zip", "ftp://h/f"]);
  });
  it("base64 and truncation", () => {
    expect(bytesToBase64(new Uint8Array([77, 97, 110]))).toBe("TWFu");
    const t = middleTruncate("a-very-long-file-name-that-keeps-going-and-going.tar.gz", 30);
    expect(t.length).toBeLessThanOrEqual(30);
    expect(t.endsWith(".gz")).toBe(true);
  });
});

describe("ring buffer & paths", () => {
  it("keeps the newest samples in order", () => {
    const r = new Ring(3);
    [1, 2, 3, 4, 5].forEach((v) => r.push(v));
    expect(r.values()).toEqual([3, 4, 5]);
    expect(r.max()).toBe(5);
    expect(r.size).toBe(3);
  });
  it("builds closed area paths", () => {
    const { line, area } = smoothPath([0, 5, 10], 100, 40);
    expect(line.startsWith("M0.0")).toBe(true);
    expect(area.endsWith("Z")).toBe(true);
    expect(smoothPath([1], 10, 10)).toEqual({ line: "", area: "" });
  });
});

describe("torrent tree", () => {
  it("groups by folder with sizes and tri-state selection", () => {
    const t = buildTree([
      { index: 0, path: "docs/a.pdf", size: 10 },
      { index: 1, path: "docs/b.pdf", size: 5 },
      { index: 2, path: "big.iso", size: 100 },
    ]);
    expect(t.size).toBe(115);
    expect(t.children.map((c) => c.name)).toEqual(["docs", "big.iso"]);
    const docs = t.children[0];
    expect(docs.size).toBe(15);
    expect(nodeState(docs, new Set([0]))).toBe("some");
    expect(nodeState(docs, new Set([0, 1]))).toBe("all");
    expect(nodeState(docs, new Set([2]))).toBe("none");
  });
});

describe("categoryForFiles (torrent window)", () => {
  const cats = [
    { id: "video", extensions: ["mp4", "mkv"] },
    { id: "compressed", extensions: [".ISO", "zip"] },
    { id: "other", extensions: [] },
  ];
  it("takes the category of the largest file", () => {
    const files = [
      { path: "Show/sample.mkv", size: 20 },
      { path: "Show/disc.iso", size: 4000 },
      { path: "Show/info.nfo", size: 1 },
    ];
    expect(categoryForFiles(files, cats)).toBe("compressed");
    expect(categoryForFiles([{ path: "movie.MKV", size: 5 }], cats)).toBe("video");
  });
  it("falls back to Other", () => {
    expect(categoryForFiles([{ path: "readme", size: 5 }], cats)).toBe("other");
    expect(categoryForFiles([], cats)).toBe("other");
  });
});

describe("formatDay", () => {
  it("shows a calendar day without a time, and passes through what it can't read", () => {
    const day = formatDay("2026-11-01");
    expect(day).toMatch(/1/);
    expect(day).not.toMatch(/:/);
    expect(formatDay("not-a-date")).toBe("not-a-date");
  });
});
