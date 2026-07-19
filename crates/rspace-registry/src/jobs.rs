//! In-memory async-job registry for long-running admin operations.
//!
//! A large class migration can copy terabytes; running it inside the admin
//! request would hold that connection open for the duration. Callers can
//! instead start it in the background (`"async": true`), get a job id back
//! immediately, and poll `GET /admin/jobs/<id>` for progress.
//!
//! The registry is process-local and non-durable — a restart forgets
//! in-flight jobs. The migration itself is idempotent and restartable, so a
//! forgotten job is recovered by re-issuing it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: String,
    pub state: JobState,
    /// Operation parameters, echoed back for context.
    pub params: serde_json::Value,
    /// The operation's report once `state == Done`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<serde_json::Value>,
    /// The failure message once `state == Failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Shared handle to the job table.
#[derive(Clone, Default)]
pub struct Jobs(Arc<Mutex<HashMap<String, JobRecord>>>);

impl Jobs {
    /// Register a new running job and return its id.
    pub fn start(&self, kind: &str, params: serde_json::Value) -> String {
        let id = Uuid::new_v4().to_string();
        let rec = JobRecord {
            id: id.clone(),
            kind: kind.to_string(),
            state: JobState::Running,
            params,
            report: None,
            error: None,
        };
        self.0.lock().unwrap().insert(id.clone(), rec);
        id
    }

    /// Mark a job finished with its report.
    pub fn finish(&self, id: &str, report: serde_json::Value) {
        if let Some(rec) = self.0.lock().unwrap().get_mut(id) {
            rec.state = JobState::Done;
            rec.report = Some(report);
        }
    }

    /// Mark a job failed with an error message.
    pub fn fail(&self, id: &str, error: String) {
        if let Some(rec) = self.0.lock().unwrap().get_mut(id) {
            rec.state = JobState::Failed;
            rec.error = Some(error);
        }
    }

    pub fn get(&self, id: &str) -> Option<JobRecord> {
        self.0.lock().unwrap().get(id).cloned()
    }

    /// All jobs, newest-registration order not guaranteed (map order).
    pub fn list(&self) -> Vec<JobRecord> {
        self.0.lock().unwrap().values().cloned().collect()
    }
}
