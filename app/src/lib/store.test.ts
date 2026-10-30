import { beforeEach, describe, expect, it } from "vitest";
import { handleEvent, useApp } from "./store";
import type { DownloadItem, Progress, Status } from "./types";

const item = (status: Status): DownloadItem => ({
  id: "a",
  kind: "http",
  url: "https://example.com/file.zip",
  fileName: "file.zip",
  saveDir: "/downloads",
  size: 1000,
  downloaded: 1000,
  status,
  category: "compressed",
  description: "",
  addedAt: 0,
  connections: 8,
  speedLimit: 0,
  trashed: false,
  position: 0,
  mirrors: [],
});
const progress = (downloaded: number): Progress => ({ id: "a", status: "downloading", downloaded, size: 1000, speed: 5000, connections: 8 });

describe("live progress (#41)", () => {
  beforeEach(() => useApp.setState({ items: { a: item("downloading") }, progress: {} }));

  it("progress that arrives after the download finished doesn't bring it back", () => {
    handleEvent({ type: "progress", payload: [progress(900)] });
    expect(useApp.getState().progress.a?.downloaded).toBe(900);
    handleEvent({ type: "itemUpdated", payload: item("completed") });
    expect(useApp.getState().progress.a).toBeUndefined();
    // A late tick: the row must stay completed.
    handleEvent({ type: "progress", payload: [progress(1000)] });
    expect(useApp.getState().progress.a).toBeUndefined();
    expect(useApp.getState().items.a.status).toBe("completed");
  });

  it("a download started again shows its progress", () => {
    handleEvent({ type: "itemUpdated", payload: item("completed") });
    handleEvent({ type: "itemUpdated", payload: item("connecting") });
    handleEvent({ type: "progress", payload: [progress(10)] });
    expect(useApp.getState().progress.a?.downloaded).toBe(10);
  });

  it("the whole list sent again drops the progress of downloads that stopped", () => {
    handleEvent({ type: "progress", payload: [progress(500)] });
    handleEvent({ type: "itemsAdded", payload: [item("completed")] });
    expect(useApp.getState().progress.a).toBeUndefined();
    expect(useApp.getState().items.a.status).toBe("completed");
  });
});
