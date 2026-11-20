//! Background job registry shared by the engine, MCP tools, and the UI.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum JobState {
    Running { progress: f64 },
    Done { message: String, data: Value },
    Failed { error: String },
    Cancelled,
}

impl JobState {
    pub fn label(&self) -> &'static str {
        match self {
            JobState::Running { .. } => "running",
            JobState::Done { .. } => "done",
            JobState::Failed { .. } => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub fn progress(&self) -> Option<f64> {
        match self {
            JobState::Running { progress } => Some(*progress),
            _ => None,
        }
    }
}

/// Work the engine should perform when it next polls a finished job
/// (importing a rendered HyperFrames asset or committing a Whisper transcript).
#[derive(Debug, Clone)]
pub enum PendingFinalize {
    ImportHyperFrames {
        asset_id: Uuid,
        overlay: bool,
        insert_at_ticks: Option<cutpilot_core::Ticks>,
        duration_ticks: cutpilot_core::Ticks,
        transparent: bool,
    },
    StoreTranscript {
        asset_id: Uuid,
        add_captions: bool,
    },
    None,
}

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: Uuid,
    /// `render` or `hyperframes`.
    pub kind: String,
    pub label: String,
    pub state: JobState,
    pub created_at: SystemTime,
    /// Set once the engine has applied the pending finalize.
    pub finalized: bool,
    pub pending: PendingFinalize,
}

impl JobInfo {
    pub fn to_json(&self) -> Value {
        let mut state = json!({ "status": self.state.label() });
        match &self.state {
            JobState::Running { progress } => {
                state["progress"] = json!(progress);
            }
            JobState::Done { message, data } => {
                state["message"] = json!(message);
                state["data"] = data.clone();
            }
            JobState::Failed { error } => {
                state["error"] = json!(error);
            }
            JobState::Cancelled => {}
        }
        json!({
            "job_id": self.id,
            "kind": self.kind,
            "label": self.label,
            "created_unix_secs": self
                .created_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "state": state,
        })
    }
}

/// Snapshot of an in-flight job (running or awaiting finalization) for UI status overlays.
#[derive(Debug, Clone)]
pub struct ActiveJobSnapshot {
    pub id: Uuid,
    pub kind: String,
    pub label: String,
    /// True once the worker subprocess succeeded and the engine is probing/inserting the output.
    pub finalizing: bool,
    pub elapsed_secs: f64,
}

#[derive(Default)]
pub struct JobRegistry {
    jobs: HashMap<Uuid, JobInfo>,
    order: Vec<Uuid>,
    cancels: HashMap<Uuid, Arc<AtomicBool>>,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a callback invoked whenever a job starts, finishes, is
    /// finalized, or is cancelled (e.g. to wake the UI event loop immediately).
    pub fn set_on_change(&mut self, cb: Arc<dyn Fn() + Send + Sync>) {
        self.on_change = Some(cb);
    }

    fn notify(&self) {
        if let Some(cb) = &self.on_change {
            cb();
        }
    }

    /// Register a new running job and return its id + cancel flag.
    pub fn start(&mut self, kind: &str, label: &str) -> (Uuid, Arc<AtomicBool>) {
        let id = Uuid::new_v4();
        let cancel = Arc::new(AtomicBool::new(false));
        self.jobs.insert(
            id,
            JobInfo {
                id,
                kind: kind.into(),
                label: label.into(),
                state: JobState::Running { progress: 0.0 },
                created_at: SystemTime::now(),
                finalized: false,
                pending: PendingFinalize::None,
            },
        );
        self.order.push(id);
        self.cancels.insert(id, cancel.clone());
        self.notify();
        (id, cancel)
    }

    pub fn set_pending(&mut self, id: &Uuid, pending: PendingFinalize) {
        if let Some(job) = self.jobs.get_mut(id) {
            job.pending = pending;
        }
    }

    pub fn set_progress(&mut self, id: &Uuid, progress: f64) {
        if let Some(job) = self.jobs.get_mut(id) {
            if matches!(job.state, JobState::Running { .. }) {
                job.state = JobState::Running {
                    progress: progress.clamp(0.0, 1.0),
                };
            }
        }
    }

    pub fn finish(&mut self, id: &Uuid, state: JobState) {
        if let Some(job) = self.jobs.get_mut(id) {
            job.state = state;
        }
        self.cancels.remove(id);
        self.notify();
    }

    pub fn get(&self, id: &Uuid) -> Option<&JobInfo> {
        self.jobs.get(id)
    }

    pub fn mark_finalized(&mut self, id: &Uuid) {
        if let Some(job) = self.jobs.get_mut(id) {
            job.finalized = true;
        }
        self.notify();
    }

    /// Return all in-flight jobs of `kind` (still running, or finished and
    /// waiting for the engine to finalize their pending timeline insert).
    pub fn active_jobs_of_kind(&self, kind: &str) -> Vec<ActiveJobSnapshot> {
        let mut out = Vec::new();
        for id in &self.order {
            let Some(job) = self.jobs.get(id) else {
                continue;
            };
            if job.kind != kind {
                continue;
            }
            let running = matches!(job.state, JobState::Running { .. });
            let finalizing = matches!(job.state, JobState::Done { .. })
                && !job.finalized
                && !matches!(job.pending, PendingFinalize::None);
            if running || finalizing {
                let elapsed_secs = job
                    .created_at
                    .elapsed()
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                out.push(ActiveJobSnapshot {
                    id: job.id,
                    kind: job.kind.clone(),
                    label: job.label.clone(),
                    finalizing,
                    elapsed_secs,
                });
            }
        }
        out
    }

    /// Jobs finished but not yet applied by the engine.
    pub fn unfinished_finalizes(&self) -> Vec<(Uuid, PendingFinalize)> {
        let mut out = Vec::new();
        for id in &self.order {
            if let Some(job) = self.jobs.get(id) {
                if job.finalized {
                    continue;
                }
                let ready = matches!(job.state, JobState::Done { .. })
                    && !matches!(job.pending, PendingFinalize::None);
                if ready {
                    out.push((*id, job.pending.clone()));
                }
            }
        }
        out
    }

    /// Cancel a running job. Returns false when the job is unknown or done.
    pub fn cancel(&mut self, id: &Uuid) -> bool {
        if let Some(cancel) = self.cancels.get(id) {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            self.notify();
            true
        } else {
            false
        }
    }

    /// Most recent jobs first, capped.
    pub fn list_json(&self, limit: usize) -> Value {
        let jobs: Vec<Value> = self
            .order
            .iter()
            .rev()
            .take(limit)
            .filter_map(|id| self.jobs.get(id).map(|j| j.to_json()))
            .collect();
        json!({ "jobs": jobs, "total": self.order.len() })
    }
}

/// Wrap a registry reference for progress callbacks on worker threads.
pub fn progress_sink(
    registry: &Arc<Mutex<JobRegistry>>,
    job_id: Uuid,
) -> Arc<dyn Fn(f64) + Send + Sync> {
    let registry = registry.clone();
    Arc::new(move |p: f64| {
        if let Ok(mut reg) = registry.lock() {
            reg.set_progress(&job_id, p);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn active_hyperframes_jobs_track_running_finalizing_and_done() {
        let mut reg = JobRegistry::new();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_c = wakes.clone();
        reg.set_on_change(Arc::new(move || {
            wakes_c.fetch_add(1, Ordering::Relaxed);
        }));

        let (id, _cancel) = reg.start("hyperframes", "Rendering HyperFrames: lower-third");
        reg.set_pending(
            &id,
            PendingFinalize::ImportHyperFrames {
                asset_id: Uuid::new_v4(),
                overlay: true,
                insert_at_ticks: Some(0),
                duration_ticks: 3_600_000,
                transparent: true,
            },
        );

        // Running -> active, not finalizing.
        let active = reg.active_jobs_of_kind("hyperframes");
        assert_eq!(active.len(), 1);
        assert!(!active[0].finalizing);
        assert!(reg.active_jobs_of_kind("render").is_empty());

        // Worker finished -> still active while awaiting finalize_hyperframes.
        reg.finish(
            &id,
            JobState::Done {
                message: "rendered".into(),
                data: json!({"path": "/tmp/render.mov"}),
            },
        );
        let active = reg.active_jobs_of_kind("hyperframes");
        assert_eq!(active.len(), 1);
        assert!(active[0].finalizing);

        // Finalized -> overlay closes!
        reg.mark_finalized(&id);
        assert!(reg.active_jobs_of_kind("hyperframes").is_empty());
        assert_eq!(wakes.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn failed_or_cancelled_hyperframes_job_clears_active_state() {
        let mut reg = JobRegistry::new();
        let (id1, _) = reg.start("hyperframes", "fail-case");
        reg.finish(&id1, JobState::Failed { error: "boom".into() });
        assert!(reg.active_jobs_of_kind("hyperframes").is_empty());

        let (id2, cancel2) = reg.start("hyperframes", "cancel-case");
        assert!(reg.cancel(&id2));
        assert!(cancel2.load(Ordering::Relaxed));
        reg.finish(&id2, JobState::Cancelled);
        assert!(reg.active_jobs_of_kind("hyperframes").is_empty());
    }
}
