import { type ReactNode, useEffect, useRef, useState } from "react";
import { IconButton } from "../components/IconButton";
import { useItemDetails } from "../components/useItemDetails";
import { ConnectionLanes, FileMap, ProgressBar, SpeedChart } from "../components/Visuals";
import { Icon } from "../design/icons";
import { actions } from "../lib/actions";
import { api } from "../lib/api";
import { formatBytes, formatDate, formatDuration, formatSpeed } from "../lib/format";
import { t, tk } from "../lib/i18n";
import { fullPath } from "../lib/paths";
import { progressOf, statusView } from "../lib/present";
import { Ring } from "../lib/ring";
import { useApp } from "../lib/store";
import type { ItemDetails, SegmentInfo } from "../lib/types";
import { useUi } from "../lib/ui";
import { copyText } from "../lib/win";
import { NETWORK_COLORS, usePolled } from "./sheets/SettingsSheet";

const TABS = [
  { id: "connections", label: tk("Connections") },
  { id: "speed", label: tk("Speed") },
  { id: "info", label: tk("Info") },
  { id: "network", label: tk("Network") },
] as const;

/** Collapsible pane under the list showing the focused download (C-style engineering view). */
export function BottomPane() {
  const open = useUi((s) => s.bottomOpen);
  const tab = useUi((s) => s.bottomTab);
  const height = useUi((s) => s.bottomHeight);
  const focus = useApp((s) => s.selection[s.selection.length - 1] ?? null);
  const item = useApp((s) => (focus ? s.items[focus] : undefined));
  const details = useItemDetails(item && item.status !== "completed" ? focus : null, 500, open);

  const startResize = (e: React.PointerEvent) => {
    e.preventDefault();
    const y0 = e.clientY;
    const h0 = height;
    const max = Math.max(180, window.innerHeight - 320);
    const move = (ev: PointerEvent) => useUi.getState().setBottom({ bottomHeight: Math.min(max, Math.max(150, h0 - (ev.clientY - y0))) });
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      document.body.classList.remove("is-resizing-v");
    };
    document.body.classList.add("is-resizing-v");
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  // The pane only appears for a selection; with nothing selected the list gets the room.
  if (!open || !item) return null;
  return (
    <section className="pane" aria-label={t("Selected download")}>
      <div className="pane__resize" onPointerDown={startResize} aria-hidden="true" />
      <div className="pane__tabs" role="tablist">
        {TABS.map((x) => (
          <button key={x.id} type="button" role="tab" aria-selected={tab === x.id} className={`pane__tab${tab === x.id ? " on" : ""}`} onClick={() => useUi.getState().setBottom({ bottomTab: x.id })}>
            {t(x.label)}
          </button>
        ))}
        <span className="spacer" />
        <span className="pane__file">
          <span className="ellipsis">{item.fileName}</span>
          {details?.segments.length ? <span className="badge badge--accent">{t("{count} conn", { count: details.segments.filter((s) => s.state === "active").length })}</span> : null}
        </span>
        <IconButton icon="external" size={15} label={t("Open details window")} onClick={() => actions.details(item.id)} />
        <IconButton icon="chevron-down" size={15} label={t("Hide pane")} onClick={() => useUi.getState().setBottom({ bottomOpen: false })} />
      </div>
      <div className="pane__body">
        {tab === "connections" ? (
          <ConnectionsTab details={details} done={item.status === "completed"} />
        ) : tab === "speed" ? (
          <SpeedTab details={details} id={item.id} />
        ) : tab === "network" ? (
          <NetworkTab />
        ) : (
          <InfoTab id={item.id} />
        )}
      </div>
    </section>
  );
}

function ConnectionsTab({ details, done }: { details: ItemDetails | null; done: boolean }) {
  if (done) return <p className="pane__hint">{t("Download complete — no open connections.")}</p>;
  if (!details) return <p className="pane__hint">{t("Loading…")}</p>;
  const size = details.item.size ?? 0;
  const unit = details.item.kind === "hls" || details.item.kind === "dash" ? "segments" : "bytes";
  return (
    <div className="pane__conn">
      <div className="pane__map">
        <span>{t("File map")}</span>
        <FileMap segments={details.segments} size={size} unit={unit} />
        <span className="tnum">{t("{count} segments", { count: details.segments.length })}</span>
      </div>
      {details.segments.some((s) => s.network) ? <NetworkLanes segments={details.segments} /> : <ConnectionLanes segments={details.segments} unit={unit} />}
    </div>
  );
}

/** This download's speed per network, with each connection coloured by the network it runs on. */
function NetworkLanes({ segments }: { segments: SegmentInfo[] }) {
  const [nets] = usePolled(api.networks, 2000);
  const view = (iface: string) => {
    const i = nets?.findIndex((n) => n.interface === iface) ?? -1;
    const n = nets?.[i];
    return { name: n?.name || iface, color: n?.color || NETWORK_COLORS[(i < 0 ? 0 : i) % NETWORK_COLORS.length] };
  };
  const active = segments.filter((s) => s.state === "active");
  const groups = new Map<string, { speed: number; count: number }>();
  for (const s of active) {
    const g = groups.get(s.network ?? "") ?? { speed: 0, count: 0 };
    groups.set(s.network ?? "", { speed: g.speed + s.speed, count: g.count + 1 });
  }
  const total = active.reduce((sum, s) => sum + s.speed, 0);
  return (
    <>
      <div className="sstats">
        {[...groups].map(([iface, g]) => {
          const v = iface ? view(iface) : null;
          return (
            <div key={iface} className="sstat">
              <div className="k ellipsis">
                {v && <span aria-hidden style={{ display: "inline-block", width: 8, height: 8, borderRadius: 4, marginInlineEnd: 5, background: v.color }} />}
                {v?.name ?? "—"} · {t("{count} conn", { count: g.count })}
              </div>
              <div className="v tnum">{formatSpeed(g.speed)}</div>
            </div>
          );
        })}
        <div className="sstat">
          <div className="k">{t("Combined speed")}</div>
          <div className="v tnum">{formatSpeed(total)}</div>
        </div>
      </div>
      <ConnectionLanes segments={segments} colorOf={(s) => (s.network ? view(s.network).color : undefined)} />
    </>
  );
}

function SpeedTab({ details, id }: { details: ItemDetails | null; id: string }) {
  const progress = useApp((s) => s.progress[id]);
  const ring = useRef(new Ring(120));
  const [, force] = useState(0);
  // Seed with the core's history, then append live progress samples.
  useEffect(() => {
    ring.current = new Ring(120);
  }, [id]);
  useEffect(() => {
    if (details && ring.current.size === 0) details.speedHistory.slice(-120).forEach((v) => ring.current.push(v));
  }, [details]);
  useEffect(() => {
    if (progress) {
      ring.current.push(progress.speed);
      force((n) => n + 1);
    }
  }, [progress]);
  const samples = ring.current.values();
  const avg = samples.length ? samples.reduce((a, b) => a + b, 0) / samples.length : 0;
  return (
    <div className="pane__speed">
      <SpeedChart samples={samples} height={140} />
      <div className="sstats">
        <Stat k={t("Now")} v={formatSpeed(progress?.speed)} />
        <Stat k={t("Average")} v={formatSpeed(avg)} />
        <Stat k={t("Peak")} v={formatSpeed(ring.current.max())} />
        <Stat k={t("Time left")} v={formatDuration(progress?.etaSecs)} />
      </div>
    </div>
  );
}

const Stat = ({ k, v }: { k: string; v: ReactNode }) => (
  <div className="sstat">
    <div className="k">{k}</div>
    <div className="v tnum">{v}</div>
  </div>
);

/** Speed over each network the downloads use (all downloads, as the engine counts per network). */
function NetworkTab() {
  const on = useApp((s) => s.settings?.networks.enabled);
  const [nets] = usePolled(api.networks, 1000);
  const used = (nets ?? []).filter((n) => n.enabled);
  if (!on || !nets || used.length === 0) return <p className="pane__hint">{t("Turn on Use multiple networks in Settings › Networks to see the speed of each network here.")}</p>;
  const total = used.reduce((sum, n) => sum + n.speed, 0);
  return (
    <dl className="kv">
      {used.map((n) => {
        const color = n.color || NETWORK_COLORS[nets.indexOf(n) % NETWORK_COLORS.length];
        const state = !n.present ? t("Not connected") : n.failing ? t("No internet on this network?") : `${formatSpeed(n.speed)} · ${t("{count} connections", { count: n.connections })} · ${formatBytes(n.received)}`;
        return (
          <div key={n.interface} className="kv__row">
            <dt>
              <span aria-hidden style={{ display: "inline-block", width: 10, height: 10, borderRadius: 5, marginInlineEnd: 6, background: color, opacity: n.present ? 1 : 0.4 }} />
              {n.name}
            </dt>
            <dd>
              <span className="tnum">{state}</span>
              <ProgressBar value={total ? n.speed / total : 0} state="active" size="thin" label={n.name} />
            </dd>
          </div>
        );
      })}
      <div className="kv__row">
        <dt>{t("Combined speed")}</dt>
        <dd className="tnum">{formatSpeed(total)}</dd>
      </div>
    </dl>
  );
}

function InfoTab({ id }: { id: string }) {
  const item = useApp((s) => s.items[id]);
  const progress = useApp((s) => s.progress[id]);
  const cat = useApp((s) => s.categories.find((c) => c.id === item?.category)?.name);
  const queue = useApp((s) => s.queues.find((q) => q.id === item?.queueId)?.name);
  if (!item) return null;
  const p = progressOf(item, progress);
  const rows: [string, ReactNode, string?][] = [
    [t("Address"), item.url, item.url],
    ...(item.finalUrl && item.finalUrl !== item.url ? ([[t("Final address"), item.finalUrl, item.finalUrl]] as [string, ReactNode, string][]) : []),
    [t("Saved to"), fullPath(item.saveDir, item.fileName), fullPath(item.saveDir, item.fileName)],
    [t("Status"), statusView(item, progress).detail],
    [t("Size"), t("{done} of {total}", { done: formatBytes(p.done), total: formatBytes(p.total) })],
    [t("Resume"), item.resumable === false ? t("Not supported — restarts from zero") : item.resumable ? t("Supported") : t("Unknown")],
    [t("Category"), cat ?? item.category],
    [t("Queue"), queue ?? "—"],
    [t("Connections"), String(item.connections)],
    [t("Speed limit"), item.speedLimit ? `${formatBytes(item.speedLimit)}/s` : t("None")],
    [t("Added"), formatDate(item.addedAt)],
    [t("Finished"), formatDate(item.finishedAt)],
    [t("Checksum"), item.checksum || "—"],
    ...(item.description ? ([[t("Description"), item.description]] as [string, ReactNode][]) : []),
  ];
  return (
    <dl className="kv">
      {rows.map(([k, v, copy]) => (
        <div key={k} className="kv__row">
          <dt>{k}</dt>
          <dd className={copy ? "t-mono selectable" : "selectable"}>
            {v}
            {copy && (
              <button type="button" className="kv__copy" aria-label={t("Copy {field}", { field: k })} data-tip={t("Copy")} onClick={() => void copyText(copy)}>
                <Icon name="copy" size={13} />
              </button>
            )}
          </dd>
        </div>
      ))}
    </dl>
  );
}
