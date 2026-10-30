import { useEffect } from "react";
import { BrandTile } from "../../components/LogoMark";
import { Icon, type IconName } from "../../design/icons";
import { patchSettings } from "../../lib/actions";
import { api } from "../../lib/api";
import { t, tk } from "../../lib/i18n";
import { openUrl, REPO_URL } from "../../lib/links";
import { openTools } from "../../lib/tools";
import { useApp } from "../../lib/store";
import { useUi } from "../../lib/ui";
import { versionNewer } from "../../lib/version";
import { BrowserConnect, useExtensionStatus } from "../BrowserConnect";

type Change = { icon: IconName; title: string; body: string; credit?: string; action?: "sounds" | "language" | "appearance" | "model" | "tools" };

/**
 * What this version brings (shown once after an update, and from About Prism), then what the
 * version before brought, for people updating from older ones. Titles and bodies are translated
 * where they're shown; `credit` is the contributor's handle.
 */
const CHANGES: Change[] = [
  {
    icon: "convert",
    title: tk("Convert and make smaller"),
    body: tk("Tools in the sidebar, in a finished download's menu and in the “Download complete” window: turn pictures, music and videos into another format, or make them small enough for Discord, e-mail or WhatsApp."),
    action: "tools",
  },
  {
    icon: "video",
    title: tk("Just a part of a video"),
    body: tk("Pick a quality with the Download button next to a video, or open the “Download video” window in Prism, and choose “Only a part”: where it starts and ends, or a chapter. Prism downloads only that part."),
  },
  {
    icon: "file-archive",
    title: tk("What's inside a ZIP"),
    body: tk("The “Download file” window shows the files in a ZIP before you download it, and can download just one of them."),
  },
  {
    icon: "status-completed",
    title: tk("Finished means finished"),
    body: tk("A download that finished while the window was open sometimes still looked like it was downloading."),
    credit: "nckrkllkc",
  },
];

/** The previous version's changes, shown below the new ones. */
const PREVIOUS = { version: "1.1.8", changes: [
  {
    icon: "batch",
    title: tk("Lists of links"),
    body: tk("A pasted list of links opens one Download links window again, not a second one that froze and couldn't be closed."),
  },
  {
    icon: "monitor",
    title: tk("Prism on Linux"),
    body: tk("On KDE Plasma, a window brought back from the taskbar no longer stays empty, and Prism gives memory back after long downloads."),
  },
  {
    icon: "eye",
    title: tk("Dock icon on the Mac"),
    body: tk("Prism can hide its Dock icon while it waits in the menu bar: Settings › General."),
    credit: "mehdiillala",
  },
] satisfies Change[] };

/**
 * "What's new" after an update: the browser extension from the Chrome Web Store first (one
 * click, updates itself; replaces one added by hand), then what else changed.
 */
export function WhatsNewSheet() {
  const version = useApp((s) => s.version);
  const { status, connected } = useExtensionStatus();
  const fromStore = connected && status?.extensionInstallType === "normal";
  return (
    <div className="whatsnew">
      <header className="whatsnew__head">
        <BrandTile size={52} />
        <div>
          <h2>{t("Prism {version} is here", { version })}</h2>
          <p className="text-secondary">{t("Convert and make smaller, just a part of a video, and a look inside ZIP files.")}</p>
        </div>
      </header>

      <section className="whatsnew__store card">
        <div className="whatsnew__storehead">
          <Icon name="puzzle" size={18} className="text-accent" />
          <div>
            <b>{t("Add Prism to your browser in one click")}</b>
            <div className="t-small text-secondary">
              {t("From the Chrome Web Store, for Chrome, Edge, Brave, Opera, Vivaldi and Helium. It updates by itself, so there's nothing to reload after a Prism update.")}
            </div>
          </div>
        </div>
        {fromStore ? (
          <div className="bstatus bstatus--ok" role="status">
            <span className="bstatus__icon">
              <Icon name="check" size={16} strokeWidth={2.4} />
            </span>
            <div>
              <b>{t("You're all set.")}</b> {t("Your browser runs Prism from the Chrome Web Store.")}
            </div>
          </div>
        ) : (
          <BrowserConnect status={status} connected={connected} />
        )}
      </section>

      <ChangeList changes={CHANGES} />
      <h3 className="whatsnew__older t-small text-tertiary">{t("New in Prism {version}", { version: PREVIOUS.version })}</h3>
      <ChangeList changes={PREVIOUS.changes} />
      <p className="whatsnew__star t-small text-secondary">
        <Icon name="star" size={14} className="text-accent" />
        <span>
          {t("Enjoying Prism? A star on GitHub helps more people find it.")}{" "}
          <button type="button" className="linkbtn" onClick={() => void openUrl(REPO_URL)}>
            {t("Star on GitHub")}
          </button>
        </span>
      </p>
    </div>
  );
}

function ChangeList({ changes }: { changes: Change[] }) {
  const openSheet = useUi((s) => s.openSheet);
  return (
    <ul className="whatsnew__list">
      {changes.map((c) => (
        <li key={c.title}>
          <span className="whatsnew__icon">
            <Icon name={c.icon} size={16} />
          </span>
          <div>
            <b>{t(c.title)}</b>
            <div className="t-small text-secondary">
              {t(c.body)}
              {c.credit && ` ${t("Thanks to {name}!", { name: c.credit })}`}{" "}
              {c.action === "sounds" && (
                <button type="button" className="linkbtn" onClick={() => openSheet({ kind: "settings", section: "sounds" })}>
                  {t("Try them")}
                </button>
              )}
              {c.action === "appearance" && (
                <button type="button" className="linkbtn" onClick={() => openSheet({ kind: "settings", section: "appearance" })}>
                  {t("Try it")}
                </button>
              )}
              {c.action === "model" && (
                <button type="button" className="linkbtn" onClick={() => void api.openWindow("model")}>
                  {t("Try it")}
                </button>
              )}
              {c.action === "tools" && (
                <button type="button" className="linkbtn" onClick={() => openTools({ tab: "convert", paths: [] })}>
                  {t("Try it")}
                </button>
              )}
              {c.action === "language" && (
                <button type="button" className="linkbtn" onClick={() => openSheet({ kind: "settings", section: "general" })}>
                  {t("Choose a language")}
                </button>
              )}
            </div>
          </div>
        </li>
      ))}
    </ul>
  );
}

/**
 * Opens "What's new" once after Prism was updated, when the main window is on screen (not while
 * it starts hidden in the tray). A fresh install shows the onboarding instead and only records
 * the version.
 */
export function useWhatsNew() {
  const version = useApp((s) => s.version);
  const seen = useApp((s) => s.settings?.lastSeenVersion);
  const onboarded = useApp((s) => s.settings?.onboardingDone);
  useEffect(() => {
    if (!version || seen === undefined || onboarded === undefined || seen === version) return;
    if (!onboarded || (seen && !versionNewer(version, seen))) {
      void patchSettings({ lastSeenVersion: version });
      return;
    }
    const show = () => {
      if (document.visibilityState !== "visible") return false;
      if (!useUi.getState().sheet) useUi.getState().openSheet({ kind: "whatsnew" });
      void patchSettings({ lastSeenVersion: version });
      return true;
    };
    if (show()) return;
    const onVisible = () => {
      if (show()) document.removeEventListener("visibilitychange", onVisible);
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  }, [version, seen, onboarded]);
}
