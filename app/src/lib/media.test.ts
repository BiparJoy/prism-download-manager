import { describe, expect, it } from "vitest";
import { describe as describeFile, kindByName, type MediaFile, planSummary, planWarning, presetFileLabel, presetLabel, quickTargets } from "./media";
import { hexEncode, parseRoute } from "./route";

const video: MediaFile = {
  path: "C:\\d\\clip.mp4",
  name: "clip.mp4",
  size: 728_000_000,
  kind: "video",
  ext: "mp4",
  width: 1920,
  height: 1080,
  duration: 596,
  fps: 60,
  hasAudio: true,
  hasAlpha: false,
  hasCover: false,
  tiled: false,
  targets: ["webm", "gif", "mp3"],
};

describe("tools", () => {
  it("opens the Tools window on the asked tab, with its files", () => {
    expect(parseRoute("#/tools/shrink")).toEqual({ name: "tools", request: { tab: "shrink" } });
    expect(parseRoute("#/tools/whatever")).toEqual({ name: "tools", request: { tab: "convert" } });
    const req = { tab: "shrink", paths: ["C:\\Users\\Ů\\clip (1).mp4"], preset: "discord" };
    expect(parseRoute(`#/tools/shrink/${hexEncode(JSON.stringify(req))}`)).toEqual({ name: "tools", request: req });
    // A broken request still opens the window.
    expect(parseRoute("#/tools/convert/zz")).toEqual({ name: "tools", request: { tab: "convert" } });
  });

  it("offers 3–4 formats by the file's name, its own left out", () => {
    expect(kindByName("Holiday.HEIC")).toBe("image");
    expect(kindByName("notes.pdf")).toBe("other");
    expect(quickTargets("song.flac")).toEqual(["mp3", "m4a", "wav"]);
    expect(quickTargets("song.mp3")).toEqual(["m4a", "wav", "flac"]);
    expect(quickTargets("clip.mkv")).toEqual(["mp4", "mp3", "webm", "gif"]);
    expect(quickTargets("clip.mp4")).toEqual(["mp3", "webm", "gif"]);
    expect(quickTargets("photo.png")).toEqual(["jpg", "webp"]);
    expect(quickTargets("setup.exe")).toEqual([]);
  });

  it("names the presets and the results", () => {
    expect(presetLabel("discord")).toBe("Discord · 20 MB");
    expect(presetLabel("email")).toBe("E-mail · 25 MB");
    expect(presetFileLabel("whatsapp")).toBe("WhatsApp");
    expect(presetFileLabel("custom")).toBe("smaller");
  });

  it("says what changes and warns in plain words", () => {
    expect(describeFile(video)).toBe("Video · 1080p · 9:56");
    const plan = { verdict: "ok" as const, grade: "low" as const, ext: "mp4", width: 640, height: 360, fps: 30, videoKbps: 189, audioKbps: 64, estSize: 19_000_000 };
    expect(planSummary(video, plan)).toBe("1080p → 360p · 30 fps · sound 64 kbps");
    expect(planWarning(video, plan, 20_000_000)).toBe("9:56 of video is a lot for 20 MB: expect a blurry picture.");
    expect(planWarning(video, { ...plan, grade: "good" }, 20_000_000)).toBe("");
    expect(planWarning(video, { verdict: "tooLong", ext: "mp4" }, 10_000_000)).toMatch(/^9:56 of video won’t fit into 10 MB/);
  });
});
