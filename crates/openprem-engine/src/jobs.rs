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
/// (importing a rendered HyperFrames asset and inserting its clip).
#[derive(Debug, Clone)]
pub enum PendingFinalize {
    ImportHyperFrames {
        asset_id: Uuid,
        overlay: bool,
        insert_at_ticks: Option<openprem_core::Ticks>,
        duration_ticks: openprem_core::Ticks,
        transparent: bool,
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

#[derive(Default)]
pub struct JobRegistry {
    jobs: HashMap<Uuid, JobInfo>,
    order: Vec<Uuid>,
    cancels: HashMap<Uuid, Arc<AtomicBool>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
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
    }

    pub fn get(&self, id: &Uuid) -> Option<&JobInfo> {
        self.jobs.get(id)
    }

    pub fn mark_finalized(&mut self, id: &Uuid) {
        if let Some(job) = self.jobs.get_mut(id) {
            job.finalized = true;
        }
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
