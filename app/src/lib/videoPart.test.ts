import { describe, expect, it } from "vitest";
import { progressOf } from "./present";
import type { DownloadItem } from "./types";
import { clampRange, defaultRange, formatRange, isWhole, keyStep, parseTime, partSize, timelineLength, untitledChapter, withEnd, withStart } from "./videoPart";

describe("typed times", () => {
  it("reads seconds, m:ss and h:mm:ss", () => {
    expect(parseTime("83")).toBe(83);
    expect(parseTime("1:23")).toBe(83);
    expect(parseTime("01:23")).toBe(83);
    expect(parseTime(" 1:02:03 ")).toBe(3723);
    expect(parseTime("90:00")).toBe(5400);
    expect(parseTime("1:23.6")).toBe(84);
    expect(parseTime("0")).toBe(0);
  });

  it("refuses what isn't a time", () => {
    for (const bad of ["", " ", "1:", ":23", "1:75", "1:2:3:4", "-5", "1,5", "abc", "1m23s", "1:2 3", "1.5:00"]) expect(parseTime(bad), bad).toBeNull();
  });

  it("shows a range as clock times", () => {
    expect(formatRange({ start: 83, end: 165 })).toBe("1:23–2:45");
    expect(formatRange({ start: 0, end: 3723 })).toBe("0:00–1:02:03");
  });
});

describe("the part's range", () => {
  const chapters = [
    { title: "Intro", start: 0, end: 23.6 },
    { title: "Main", start: 23.6, end: 400 },
  ];

  it("starts on the first chapter, else the first minute", () => {
    expect(defaultRange(635, chapters)).toEqual({ start: 0, end: 24 });
    expect(defaultRange(635, [])).toEqual({ start: 0, end: 60 });
    // All of a shorter video: that's the whole video, a normal download.
    expect(defaultRange(45, [])).toEqual({ start: 0, end: 45 });
    expect(isWhole(defaultRange(45, []), 45)).toBe(true);
    expect(timelineLength(634.566)).toBe(635);
  });

  it("stays on the video, in order, at least a second long", () => {
    expect(clampRange(-5, 30, 100)).toEqual({ start: 0, end: 30 });
    expect(clampRange(90, 200, 100)).toEqual({ start: 90, end: 100 });
    expect(clampRange(50, 50, 100)).toEqual({ start: 50, end: 51 });
    expect(clampRange(100, 100, 100)).toEqual({ start: 99, end: 100 });
  });

  it("moves along when a typed start passes the end (or an end the start)", () => {
    const r = { start: 60, end: 120 };
    expect(withStart(r, 90, 600)).toEqual({ start: 90, end: 120 });
    expect(withStart(r, 300, 600)).toEqual({ start: 300, end: 360 });
    expect(withStart(r, 580, 600)).toEqual({ start: 580, end: 600 });
    expect(withEnd(r, 100, 600)).toEqual({ start: 60, end: 100 });
    expect(withEnd(r, 30, 600)).toEqual({ start: 0, end: 30 });
    expect(withEnd(r, 1000, 600)).toEqual({ start: 60, end: 600 });
  });

  it("moves a handle by keys: 1 s, 10 s with Shift, to the ends with Home/End", () => {
    expect(keyStep("ArrowRight", false, 10, 0, 50)).toBe(11);
    expect(keyStep("ArrowLeft", true, 10, 0, 50)).toBe(0);
    expect(keyStep("ArrowUp", true, 45, 0, 50)).toBe(50);
    expect(keyStep("PageDown", false, 30, 0, 50)).toBe(20);
    expect(keyStep("Home", false, 30, 5, 50)).toBe(5);
    expect(keyStep("End", false, 30, 5, 50)).toBe(50);
    expect(keyStep("a", false, 30, 0, 50)).toBeNull();
  });

  it("estimates the part's size from the format's", () => {
    expect(partSize(600_000_000, { start: 60, end: 120 }, 600)).toBe(60_000_000);
    expect(partSize(null, { start: 60, end: 120 }, 600)).toBeNull();
  });

  it("numbers chapters yt-dlp couldn't name", () => {
    expect(untitledChapter("<Untitled Chapter 3>")).toBe(3);
    expect(untitledChapter("Intro")).toBeNull();
  });
});

describe("progress of a part", () => {
  const item: DownloadItem = {
    id: "p",
    kind: "video",
    url: "https://www.youtube.com/watch?v=x",
    fileName: "Clip (1m23s–2m45s).mp4",
    saveDir: "C:\\Videos",
    size: 400_000_000, // an earlier attempt's: not this part's
    downloaded: 0,
    status: "downloading",
    category: "video",
    description: "",
    addedAt: 0,
    connections: 1,
    speedLimit: 0,
    trashed: false,
    position: 0,
    mirrors: [],
  };

  it("follows the share done while the size is unknown, never past 100%", () => {
    const p = progressOf(item, { id: "p", status: "downloading", downloaded: 3_000_000, size: null, speed: 0, connections: 0, fraction: 0.4 });
    expect(p.pct).toBeCloseTo(40);
    expect(p.total).toBeNull();
    expect(p.done).toBe(3_000_000);
    expect(progressOf(item, { id: "p", status: "downloading", downloaded: 1, size: null, speed: 0, connections: 0, fraction: 1.3 }).pct).toBe(100);
    // Done: the file's size again.
    expect(progressOf({ ...item, status: "completed", size: 9_000_000 }, null).pct).toBe(100);
  });
});
