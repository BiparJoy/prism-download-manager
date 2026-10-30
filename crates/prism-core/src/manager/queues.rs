//! Queues, categories, schedule evaluation and post-queue actions.

use super::Core;
use crate::events::CoreEvent;
use crate::model::*;
use crate::platform::{self, PowerAction};
use crate::scheduler;
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::time::Duration;

/// Seconds the user gets to cancel a shutdown/sleep/hibernate after a queue finishes.
const POST_ACTION_GRACE_SECS: u32 = 30;

impl Core {
    // ----- categories ---------------------------------------------------------------------------

    pub fn upsert_category(&self, mut c: Category) -> Result<Vec<Category>> {
        if c.id.is_empty() {
            c.id = uuid::Uuid::new_v4().simple().to_string();
            c.position = 100 + self.0.state.lock().categories.len() as i64;
            if c.icon.is_empty() {
                c.icon = "cat-custom".into();
            }
        }
        c.extensions = c
            .extensions
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        self.0.db.upsert_category(&c)?;
        let cats = {
            let mut st = self.0.state.lock();
            match st.categories.iter_mut().find(|x| x.id == c.id) {
                Some(existing) => *existing = c,
                None => st.categories.push(c),
            }
            st.categories.sort_by_key(|c| c.position);
            st.categories.clone()
        };
        self.emit(CoreEvent::CategoriesChanged(cats.clone()));
        Ok(cats)
    }

    pub fn delete_category(&self, id: &str) -> Result<Vec<Category>> {
        let builtin = self
            .0
            .state
            .lock()
            .categories
            .iter()
            .any(|c| c.id == id && c.builtin);
        if builtin {
            return Err(anyhow!("Built-in categories cannot be deleted"));
        }
        self.0.db.delete_category(id)?;
        let cats = {
            let mut st = self.0.state.lock();
            st.categories.retain(|c| c.id != id);
            for item in st.items.values_mut().filter(|i| i.category == id) {
                item.category = crate::categories::OTHER.into();
            }
            st.categories.clone()
        };
        self.emit(CoreEvent::CategoriesChanged(cats.clone()));
        Ok(cats)
    }

    // ----- queues -------------------------------------------------------------------------------

    pub fn queues(&self) -> Vec<Queue> {
        self.0.state.lock().queues.clone()
    }

    pub(crate) fn queue_running(&self, id: &str) -> bool {
        self.0
            .state
            .lock()
            .queues
            .iter()
            .any(|q| q.id == id && q.running)
    }

    fn save_queues(&self) {
        let queues = self.queues();
        for q in &queues {
            if let Err(e) = self.0.db.upsert_queue(q) {
                tracing::error!("save queue {}: {e}", q.id);
            }
        }
        self.emit(CoreEvent::QueuesChanged(queues));
    }

    pub fn upsert_queue(&self, mut q: Queue) -> Result<Vec<Queue>> {
        {
            let mut st = self.0.state.lock();
            if q.id.is_empty() {
                q.id = uuid::Uuid::new_v4().simple().to_string();
                q.position = st.queues.len() as i64;
            }
            q.max_concurrent = q.max_concurrent.clamp(1, 32);
            match st.queues.iter_mut().find(|x| x.id == q.id) {
                Some(existing) => {
                    q.builtin = existing.builtin;
                    q.running = existing.running;
                    *existing = q.clone();
                }
                None => st.queues.push(q.clone()),
            }
        }
        if let Some(l) = self.0.queue_limiters.lock().get(&q.id) {
            l.set_rate(q.speed_limit);
        }
        self.save_queues();
        Ok(self.queues())
    }

    pub fn delete_queue(&self, id: &str) -> Result<Vec<Queue>> {
        if self
            .0
            .state
            .lock()
            .queues
            .iter()
            .any(|q| q.id == id && q.builtin)
        {
            return Err(anyhow!("The main queue cannot be deleted"));
        }
        self.stop_queue(id);
        self.0.db.delete_queue(id)?;
        let ids: Vec<String> = {
            let mut st = self.0.state.lock();
            st.queues.retain(|q| q.id != id);
            st.items
                .values()
                .filter(|i| i.queue_id.as_deref() == Some(id))
                .map(|i| i.id.clone())
                .collect()
        };
        for item in ids {
            self.with_item(&item, |i| i.queue_id = None);
        }
        self.0.queue_limiters.lock().remove(id);
        self.save_queues();
        Ok(self.queues())
    }

    /// Limiter shared by all downloads of a queue.
    pub(crate) fn queue_limiter(&self, queue_id: &str) -> Arc<prism_engine::RateLimiter> {
        let rate = self
            .0
            .state
            .lock()
            .queues
            .iter()
            .find(|q| q.id == queue_id)
            .map(|q| q.speed_limit)
            .unwrap_or(0);
        self.0
            .queue_limiters
            .lock()
            .entry(queue_id.to_string())
            .or_insert_with(|| Arc::new(prism_engine::RateLimiter::new(rate)))
            .clone()
    }

    /// Put items into a queue (appended in the given order).
    pub fn assign_to_queue(&self, ids: &[String], queue_id: Option<String>) {
        let first = self.next_position(queue_id.as_deref());
        let running = queue_id.as_deref().is_some_and(|q| self.queue_running(q));
        for (pos, id) in (first..).zip(ids) {
            self.with_item(id, |i| {
                i.queue_id = queue_id.clone();
                i.position = pos;
                if running && matches!(i.status, Status::Idle | Status::Paused) {
                    i.status = Status::Queued;
                }
            });
        }
    }

    /// Move items up/down inside their queue (`delta` = -1 up, +1 down, large values = to edge).
    pub fn move_in_queue(&self, ids: &[String], delta: i64) {
        let Some(first) = ids.first().and_then(|id| self.item(id)) else {
            return;
        };
        let queue = first.queue_id.clone();
        let mut order: Vec<(String, i64)> = {
            let st = self.0.state.lock();
            st.items
                .values()
                .filter(|i| i.queue_id == queue && !i.trashed)
                .map(|i| (i.id.clone(), i.position))
                .collect()
        };
        order.sort_by_key(|(_, p)| *p);
        let mut list: Vec<String> = order.into_iter().map(|(id, _)| id).collect();
        let moving: Vec<String> = list.iter().filter(|id| ids.contains(id)).cloned().collect();
        if moving.is_empty() {
            return;
        }
        let first_idx = list.iter().position(|id| *id == moving[0]).unwrap_or(0) as i64;
        list.retain(|id| !moving.contains(id));
        let insert_at = (first_idx + delta).clamp(0, list.len() as i64) as usize;
        for (k, id) in moving.into_iter().enumerate() {
            list.insert(insert_at + k, id);
        }
        for (pos, id) in list.iter().enumerate() {
            let pos = pos as i64 + 1;
            if self.item(id).is_some_and(|i| i.position != pos) {
                self.with_item(id, |i| i.position = pos);
            }
        }
    }

    pub fn start_queue(&self, id: &str) {
        let _rt = crate::rt::enter();
        let retry_failed = {
            let mut st = self.0.state.lock();
            let Some(q) = st.queues.iter_mut().find(|q| q.id == id) else {
                return;
            };
            q.running = true;
            st.queues
                .iter()
                .find(|q| q.id == id)
                .map(|q| q.schedule.retry_failed)
                .unwrap_or(false)
        };
        let ids: Vec<String> = {
            let st = self.0.state.lock();
            st.items
                .values()
                .filter(|i| i.queue_id.as_deref() == Some(id) && !i.trashed)
                .filter(|i| {
                    matches!(i.status, Status::Idle | Status::Paused)
                        || (retry_failed && i.status == Status::Failed)
                })
                .map(|i| i.id.clone())
                .collect()
        };
        for item in ids {
            self.with_item(&item, |i| i.status = Status::Queued);
        }
        // Only now does the queue count as busy: the scheduler's tick could otherwise see it
        // busy with nothing queued yet, report it finished at once and leave its downloads
        // queued in a stopped queue.
        self.0.state.lock().queue_was_busy.insert(id.to_string());
        self.save_queues();
        self.admit();
    }

    pub fn stop_queue(&self, id: &str) {
        {
            let mut st = self.0.state.lock();
            if let Some(q) = st.queues.iter_mut().find(|q| q.id == id) {
                q.running = false;
            }
            st.queue_was_busy.remove(id);
        }
        let (active, waiting): (Vec<String>, Vec<String>) = {
            let st = self.0.state.lock();
            let in_queue = st
                .items
                .values()
                .filter(|i| i.queue_id.as_deref() == Some(id));
            let (a, w): (Vec<&DownloadItem>, Vec<&DownloadItem>) = in_queue
                .filter(|i| i.status.is_active() || i.status == Status::Queued)
                .partition(|i| i.status.is_active());
            (
                a.iter().map(|i| i.id.clone()).collect(),
                w.iter().map(|i| i.id.clone()).collect(),
            )
        };
        self.pause_items(&active);
        for item in waiting {
            self.with_item(&item, |i| i.status = Status::Idle);
        }
        self.save_queues();
    }

    /// Start queued items while their queue has free slots.
    fn admit(&self) {
        // Counting free slots and starting the items is one step: `start_queue` and the 1 s
        // tick can admit at the same moment and would otherwise both fill the same slots.
        let _admission = self.0.admission.lock();
        let to_start: Vec<String> = {
            let st = self.0.state.lock();
            let mut out = Vec::new();
            for q in st.queues.iter().filter(|q| q.running) {
                let active = st
                    .items
                    .values()
                    .filter(|i| {
                        i.queue_id.as_deref() == Some(&q.id)
                            && (i.status.is_active() || st.jobs.contains_key(&i.id))
                    })
                    .count();
                let free = (q.max_concurrent as usize).saturating_sub(active);
                if free == 0 {
                    continue;
                }
                let mut waiting: Vec<&DownloadItem> = st
                    .items
                    .values()
                    .filter(|i| {
                        i.queue_id.as_deref() == Some(&q.id)
                            && i.status == Status::Queued
                            && !i.trashed
                    })
                    .collect();
                waiting.sort_by_key(|i| i.position);
                out.extend(waiting.into_iter().take(free).map(|i| i.id.clone()));
            }
            out
        };
        if !to_start.is_empty() {
            self.start_items(&to_start);
        }
    }

    /// Called every second: schedules, admission, queue completion.
    pub(crate) fn queue_tick(&self) {
        let now = chrono::Local::now().naive_local();
        let (starts, stops) = {
            let mut st = self.0.state.lock();
            let prev = st.last_schedule_tick;
            st.last_schedule_tick = now;
            let mut starts = Vec::new();
            let mut stops = Vec::new();
            for q in &st.queues {
                let due = scheduler::due(&q.schedule, prev, now);
                if due.start && !q.running {
                    starts.push(q.id.clone());
                }
                if due.stop && q.running {
                    stops.push(q.id.clone());
                }
            }
            (starts, stops)
        };
        for id in starts {
            tracing::info!(queue = %id, "schedule: starting queue");
            self.start_queue(&id);
        }
        for id in stops {
            tracing::info!(queue = %id, "schedule: stopping queue");
            self.stop_queue(&id);
        }
        self.admit();
        self.detect_finished_queues();
    }

    fn detect_finished_queues(&self) {
        let finished: Vec<Queue> = {
            let mut st = self.0.state.lock();
            let mut done = Vec::new();
            let busy_ids: Vec<String> = st.queue_was_busy.iter().cloned().collect();
            for qid in busy_ids {
                let pending = st.items.values().any(|i| {
                    i.queue_id.as_deref() == Some(qid.as_str())
                        && !i.trashed
                        && (i.status == Status::Queued
                            || i.status.is_active()
                            || st.jobs.contains_key(&i.id))
                });
                if !pending {
                    st.queue_was_busy.remove(&qid);
                    if let Some(q) = st.queues.iter_mut().find(|q| q.id == qid) {
                        q.running = false;
                        done.push(q.clone());
                    }
                }
            }
            done
        };
        for q in finished {
            self.save_queues();
            tracing::info!(queue = %q.id, "queue finished");
            self.emit(CoreEvent::QueueFinished {
                queue_id: q.id.clone(),
                name: q.name.clone(),
            });
            let s = self.settings();
            if s.sounds.enabled && s.sounds.on_queue_done {
                self.emit(CoreEvent::PlaySound {
                    kind: "queue".into(),
                });
            }
            if q.schedule.enabled || q.schedule.on_complete != PostAction::None {
                self.run_post_action(q.schedule.on_complete.clone());
            }
            if q.schedule.once_date.is_some() {
                let mut q2 = q.clone();
                q2.schedule.enabled = false;
                let _ = self.upsert_queue(q2);
            }
        }
    }

    /// Run a post-queue action. Power actions wait for a grace period the user can cancel.
    pub fn run_post_action(&self, action: PostAction) {
        let _rt = crate::rt::enter();
        match action {
            PostAction::None => {}
            PostAction::Sound => self.emit(CoreEvent::PlaySound {
                kind: "queue".into(),
            }),
            PostAction::Command(cmd) => {
                if let Err(e) = platform::run_shell(&cmd) {
                    tracing::warn!("post action command failed: {e}");
                }
            }
            PostAction::Exit => self.emit(CoreEvent::ExitRequested),
            power => {
                let token = tokio_util::sync::CancellationToken::new();
                *self.0.post_action_cancel.lock() = Some(token.clone());
                self.emit(CoreEvent::PostActionPending {
                    action: power.clone(),
                    seconds: POST_ACTION_GRACE_SECS,
                });
                let core = self.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = token.cancelled() => tracing::info!("post action cancelled"),
                        _ = tokio::time::sleep(Duration::from_secs(POST_ACTION_GRACE_SECS as u64)) => {
                            core.checkpoint().await;
                            let a = match power {
                                PostAction::Sleep => PowerAction::Sleep,
                                PostAction::Hibernate => PowerAction::Hibernate,
                                PostAction::Shutdown => PowerAction::Shutdown,
                                _ => PowerAction::Lock,
                            };
                            if let Err(e) = platform::power(a) {
                                tracing::error!("power action failed: {e}");
                            }
                        }
                    }
                });
            }
        }
    }

    pub fn cancel_post_action(&self) {
        if let Some(t) = self.0.post_action_cancel.lock().take() {
            t.cancel();
        }
    }

    /// Next scheduled start of each queue (for the scheduler UI) and OS wake timers.
    pub fn next_schedule_runs(&self) -> Vec<(String, Option<String>)> {
        let now = chrono::Local::now().naive_local();
        self.queues()
            .iter()
            .map(|q| {
                (
                    q.id.clone(),
                    scheduler::next_start(&q.schedule, now)
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string()),
                )
            })
            .collect()
    }

    /// Register OS wake timers for schedules that ask for it.
    pub fn arm_wake_timers(&self) {
        let now = chrono::Local::now();
        for q in self
            .queues()
            .iter()
            .filter(|q| q.schedule.enabled && q.schedule.wake)
        {
            if let Some(next) = scheduler::next_start(&q.schedule, now.naive_local())
                && let Some(local) = next.and_local_timezone(chrono::Local).single()
            {
                let _ = platform::schedule_wake(local.timestamp() - 30);
            }
        }
    }
}
