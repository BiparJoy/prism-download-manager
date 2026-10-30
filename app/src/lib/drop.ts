// Dropped links / .torrent files → downloads (main window and drop zone).

import { actions } from "./actions";
import { api } from "./api";
import { t } from "./i18n";
import { bytesToBase64, categoryForFiles, extractUrls } from "./present";
import { useApp } from "./store";
import { attempt, toast } from "./ui";

/** Shared by the main window and the drop zone. */
export async function handleDrop(dt: DataTransfer) {
  const files = Array.from(dt.files).filter((f) => f.name.toLowerCase().endsWith(".torrent"));
  for (const f of files) {
    const value = bytesToBase64(new Uint8Array(await f.arrayBuffer()));
    const meta = await attempt(api.inspectTorrent({ type: "file", value }), t("Could not read {name}", { name: f.name }));
    if (meta) {
      const item = await attempt(api.addTorrent({ source: { type: "file", value }, name: meta.name, size: meta.totalSize, category: categoryForFiles(meta.files, useApp.getState().categories) }), t("Could not add the torrent"));
      if (item) toast(t("Torrent added"), { kind: "success", body: meta.name });
    }
  }
  const text = dt.getData("text/uri-list") || dt.getData("text/plain");
  const urls = extractUrls(text);
  if (urls.length === 1) await actions.addUrl(urls[0]);
  // The core opens the Download Links window for the batch itself (#37).
  else if (urls.length > 1) await attempt(api.batchFromText(urls.join("\n")), t("Could not add the links"));
  else if (!files.length && text.trim()) toast(t("Nothing to download"), { kind: "error", body: t("The dropped text contains no http(s), ftp or magnet address.") });
}

