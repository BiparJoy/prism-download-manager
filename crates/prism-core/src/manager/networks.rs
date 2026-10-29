//! Multi-network downloads and the data allowance.

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::Core;
use crate::events::CoreEvent;
use crate::model::Status;
use crate::networks::{DataUsage, engine_specs, next_period_start, used_by_default};
use crate::settings::{AllowancePeriod, NetworkEntry};

/// One network as the Networks panel shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkView {
    pub interface: String,
    /// The user's name for it, or the adapter name.
    pub name: String,
    pub color: String,
    pub addrs: Vec<String>,
    /// The adapter is connected right now.
    pub present: bool,
    pub enabled: bool,
    pub speed_limit: u64,
    /// Skipped for a while after its connections kept failing (no internet on it?).
    pub failing: bool,
    pub connections: u32,
    /// Current download speed over it in bytes/s.
    pub speed: u64,
    /// Bytes received over it since the app started.
    pub received: u64,
}

/// Data allowance state for the UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataUsageView {
    pub enabled: bool,
    pub period: AllowancePeriod,
    pub limit: u64,
    pub used: u64,
    /// Downloads are held until the period resets.
    pub exhausted: bool,
    /// The day the allowance resets (`YYYY-MM-DD`).
    pub resets_on: String,
}

/// Per-network speed meter state (last byte count, smoothed speed).
#[derive(Debug, Default)]
pub(crate) struct NetMeters {
    last: HashMap<String, (u64, f64)>,
}

fn today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

impl Core {
    /// Pushes the multi-network settings (and the adapters present now) to the engine. Runs
    /// on settings changes and every few seconds, so adapters that come and go are picked up.
    pub(crate) fn apply_networks(&self) {
        let settings = self.settings().networks;
        let specs = engine_specs(&settings, &prism_engine::list_interfaces());
        self.0.engine.networks().configure(&specs);
    }

    /// Every network the panel lists: present adapters and configured ones, in the user's
    /// order (configured first), with live statistics.
    pub fn networks(&self) -> Vec<NetworkView> {
        let settings = self.settings().networks;
        let present = prism_engine::list_interfaces();
        let stats = self.0.engine.networks().stats();
        let meters = self.0.net_meters.lock();
        let mut names: Vec<String> = settings
            .entries
            .iter()
            .map(|e| e.interface.clone())
            .collect();
        for i in &present {
            if !names.contains(&i.name) {
                names.push(i.name.clone());
            }
        }
        names
            .into_iter()
            .map(|interface| {
                let entry = settings.entry(&interface).cloned().unwrap_or(NetworkEntry {
                    interface: interface.clone(),
                    enabled: used_by_default(&interface),
                    ..NetworkEntry::default()
                });
                let adapter = present.iter().find(|i| i.name == interface);
                let st = stats.iter().find(|s| s.interface == interface);
                NetworkView {
                    name: if entry.name.is_empty() {
                        interface.clone()
                    } else {
                        entry.name.clone()
                    },
                    color: entry.color.clone(),
                    addrs: adapter
                        .map(|a| a.addrs.iter().map(|ip| ip.to_string()).collect())
                        .unwrap_or_default(),
                    present: adapter.is_some(),
                    enabled: entry.enabled,
                    speed_limit: entry.speed_limit,
                    failing: st.is_some_and(|s| s.failing),
                    connections: st.map_or(0, |s| s.connections),
                    speed: meters.last.get(&interface).map_or(0, |(_, v)| *v as u64),
                    received: st.map_or(0, |s| s.received),
                    interface,
                }
            })
            .collect()
    }

    /// Creates or changes one network's entry (name, colour, on/off, speed limit).
    pub fn update_network(&self, entry: NetworkEntry) -> Result<Vec<NetworkView>> {
        let mut networks = self.settings().networks;
        match networks
            .entries
            .iter_mut()
            .find(|e| e.interface == entry.interface)
        {
            Some(e) => *e = entry,
            None => networks.entries.push(entry),
        }
        self.update_settings(&serde_json::json!({ "networks": networks }))?;
        Ok(self.networks())
    }

    /// Samples per-network speeds (called from the 250 ms progress tick).
    pub(crate) fn sample_networks(&self, dt_secs: f64) {
        if dt_secs <= 0.0 {
            return;
        }
        let stats = self.0.engine.networks().stats();
        let mut meters = self.0.net_meters.lock();
        for s in &stats {
            let (last, speed) = meters
                .last
                .entry(s.interface.clone())
                .or_insert((s.received, 0.0));
            let now = s.received.saturating_sub(*last) as f64 / dt_secs;
            // Light smoothing so the panel does not flicker.
            *speed = *speed * 0.6 + now * 0.4;
            *last = s.received;
        }
        meters
            .last
            .retain(|k, _| stats.iter().any(|s| &s.interface == k));
    }

    // ----- data allowance ---------------------------------------------------------------------

    pub(crate) fn load_data_usage(db: &crate::db::Db, period: AllowancePeriod) -> DataUsage {
        db.get_setting("data_usage")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| DataUsage::new(period, today()))
    }

    pub(crate) fn load_allowance_held(db: &crate::db::Db) -> HashMap<String, bool> {
        db.get_setting("data_allowance_held")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save_data_usage(&self) {
        let usage = *self.0.data_usage.lock();
        if let Ok(json) = serde_json::to_string(&usage) {
            let _ = self.0.db.set_setting("data_usage", &json);
        }
        let held = self.0.allowance_held.lock().clone();
        if let Ok(json) = serde_json::to_string(&held) {
            let _ = self.0.db.set_setting("data_allowance_held", &json);
        }
    }

    /// The note on a download the allowance holds.
    pub(crate) fn allowance_note(&self) -> String {
        format!(
            "Data allowance used up, resets on {}",
            self.data_usage().resets_on
        )
    }

    pub fn data_usage(&self) -> DataUsageView {
        let a = self.settings().data_allowance;
        let mut usage = self.0.data_usage.lock();
        usage.roll(a.period, today());
        DataUsageView {
            enabled: a.enabled,
            period: a.period,
            limit: a.limit,
            used: usage.used,
            exhausted: a.enabled && usage.used >= a.limit,
            resets_on: next_period_start(a.period, today()).to_string(),
        }
    }

    /// Starts counting the current period from zero.
    pub fn reset_data_usage(&self) -> DataUsageView {
        let period = self.settings().data_allowance.period;
        *self.0.data_usage.lock() = DataUsage::new(period, today());
        self.save_data_usage();
        self.data_usage()
    }

    /// Whether the allowance is used up (new downloads wait until it resets).
    pub(crate) fn allowance_exhausted(&self) -> bool {
        self.data_usage().exhausted
    }

    /// Adds downloaded bytes to the period's usage; pauses everything when the allowance runs
    /// out. Called from the progress tick with the bytes downloaded since the last tick.
    pub(crate) fn account_data(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let before = self.data_usage();
        self.0.data_usage.lock().used += bytes;
        if before.enabled && !before.exhausted && self.allowance_exhausted() {
            self.hold_for_allowance();
        }
    }

    /// Runs every few seconds: holds what still runs while the allowance is used up (it was
    /// just switched on or lowered below the usage), and resumes the held downloads once there
    /// is room again (a new period, a higher limit, a reset, or the allowance switched off).
    pub(crate) fn enforce_allowance(&self) {
        if self.allowance_exhausted() {
            self.hold_for_allowance();
        } else {
            self.release_allowance_hold();
        }
    }

    fn release_allowance_hold(&self) {
        let held = std::mem::take(&mut *self.0.allowance_held.lock());
        if held.is_empty() {
            return;
        }
        let mut start = Vec::new();
        for (id, was_queued) in held {
            // Downloads the user resumed, removed or finished meanwhile are left alone.
            let waiting = self.with_item(&id, |i| {
                let waiting = !i.trashed && i.status == Status::Paused;
                if waiting {
                    i.error = None;
                    if was_queued {
                        i.status = Status::Queued;
                    }
                }
                waiting
            });
            if waiting == Some(true) && !was_queued {
                start.push(id);
            }
        }
        self.start_items(&start);
        self.save_data_usage();
    }

    /// Pauses every running download because the allowance is used up.
    pub(crate) fn hold_for_allowance(&self) {
        let view = self.data_usage();
        let running: Vec<String> = {
            let st = self.0.state.lock();
            let held = self.0.allowance_held.lock();
            st.items
                .values()
                .filter(|i| i.status.is_active() && !held.contains_key(&i.id))
                .map(|i| i.id.clone())
                .collect()
        };
        if running.is_empty() {
            return;
        }
        self.0
            .allowance_held
            .lock()
            .extend(running.iter().map(|id| (id.clone(), false)));
        self.pause_items(&running);
        let note = self.allowance_note();
        for id in &running {
            self.with_item(id, |i| {
                if i.status == Status::Paused || i.status.is_active() {
                    i.error = Some(note.clone());
                }
            });
        }
        self.save_data_usage();
        self.emit(CoreEvent::Notify {
            title: "Data allowance reached".into(),
            body: format!("Downloads are paused until {}.", view.resets_on),
        });
    }
}
