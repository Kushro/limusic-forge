//! The playlist job queue's storage: jobs, their items, and the lock that lets one process run
//! them at a time. Port of PlaylistForge's `pf-core/src/db/repo/{jobs,runner_lock}.rs`; the
//! runner, the executors and the controls arrive with commit 25.
//!
//! Dates are UTC RFC 3339 text ([`crate::db::rfc3339_text`]); every function takes `now` from its
//! caller so the tests can pin it.
// used by commit 25+ (runner, control, planner and the jobs page).
#![allow(dead_code)]

pub mod lock;
pub mod repo;

/// `jobs.priority`. Lower runs first; 0 is for jobs the app queues itself (an undo, a backup).
pub const PRIORITY_SYSTEM: i64 = 0;
pub const PRIORITY_HIGH: i64 = 1;
pub const PRIORITY_NORMAL: i64 = 2;
pub const PRIORITY_LOW: i64 = 3;

/// The key in `jobs.params_json` that names the engine a job runs on (`ytdata` or `innertube`,
/// D24). Written and read by commit 25; the column only has to carry it.
pub const ENGINE_KEY: &str = "engine";

/// Item actions the repo itself has to recognise.
pub mod action_name {
    /// The move barrier's own check that the copies landed. Re-running it is bookkeeping, not a
    /// retry the user would want to see counted.
    pub const VERIFY_DESTINATION: &str = "verify_destination";
}

/// `jobs.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobStatus {
    Queued,
    Running,
    PausedUser,
    WaitingQuota,
    WaitingAuth,
    PausedNetwork,
    Verifying,
    Completed,
    CompletedWithErrors,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub const ALL: [JobStatus; 11] = [
        JobStatus::Queued,
        JobStatus::Running,
        JobStatus::PausedUser,
        JobStatus::WaitingQuota,
        JobStatus::WaitingAuth,
        JobStatus::PausedNetwork,
        JobStatus::Verifying,
        JobStatus::Completed,
        JobStatus::CompletedWithErrors,
        JobStatus::Failed,
        JobStatus::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::PausedUser => "paused_user",
            JobStatus::WaitingQuota => "waiting_quota",
            JobStatus::WaitingAuth => "waiting_auth",
            JobStatus::PausedNetwork => "paused_network",
            JobStatus::Verifying => "verifying",
            JobStatus::Completed => "completed",
            JobStatus::CompletedWithErrors => "completed_with_errors",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }

    /// Done for good: the job will not change state again on its own.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Completed
                | JobStatus::CompletedWithErrors
                | JobStatus::Failed
                | JobStatus::Cancelled
        )
    }
}

/// `job_items.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobItemStatus {
    Pending,
    InFlight,
    Done,
    Failed,
    Skipped,
}

impl JobItemStatus {
    pub const ALL: [JobItemStatus; 5] = [
        JobItemStatus::Pending,
        JobItemStatus::InFlight,
        JobItemStatus::Done,
        JobItemStatus::Failed,
        JobItemStatus::Skipped,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            JobItemStatus::Pending => "pending",
            JobItemStatus::InFlight => "in_flight",
            JobItemStatus::Done => "done",
            JobItemStatus::Failed => "failed",
            JobItemStatus::Skipped => "skipped",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }
}

/// `jobs.kind`. A kind this build does not know survives a read as [`JobKind::Other`], so a
/// newer build's (or PlaylistForge's) jobs are never lost.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JobKind {
    CopyItems,
    MoveItems,
    RemoveItems,
    Reorder,
    CreatePlaylist,
    RenamePlaylist,
    Other(String),
}

impl JobKind {
    pub fn as_str(&self) -> &str {
        match self {
            JobKind::CopyItems => "copy_items",
            JobKind::MoveItems => "move_items",
            JobKind::RemoveItems => "remove_items",
            JobKind::Reorder => "reorder",
            JobKind::CreatePlaylist => "create_playlist",
            JobKind::RenamePlaylist => "rename_playlist",
            JobKind::Other(raw) => raw,
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "copy_items" => JobKind::CopyItems,
            "move_items" => JobKind::MoveItems,
            "remove_items" => JobKind::RemoveItems,
            "reorder" => JobKind::Reorder,
            "create_playlist" => JobKind::CreatePlaylist,
            "rename_playlist" => JobKind::RenamePlaylist,
            other => JobKind::Other(other.to_string()),
        }
    }
}

/// A `jobs` row, `params_json` parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: i64,
    /// The Data API channel that runs it. `None` for a job with no account, or one whose account
    /// was disconnected (`ON DELETE SET NULL`).
    pub account_id: Option<String>,
    pub kind: JobKind,
    pub params: serde_json::Value,
    pub status: JobStatus,
    pub priority: i64,
    pub phase: i64,
    pub total_phases: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub resume_at: Option<chrono::DateTime<chrono::Utc>>,
    pub est_units_total: i64,
    pub spent_units: i64,
    pub total_items: i64,
    pub done_items: i64,
    pub failed_items: i64,
    pub skipped_items: i64,
    /// The first phase's item count, frozen at insert: the "N videos" the user asked for. Unlike
    /// `total_items` it does not grow when later phases are appended.
    pub planned_items: i64,
    /// Items that took more than one attempt (the verify barrier's own re-checks excluded).
    pub retried_items: i64,
    pub last_error: Option<String>,
}

impl Job {
    /// `params_json.engine`, if the job was given one.
    pub fn engine(&self) -> Option<&str> {
        self.params.get(ENGINE_KEY).and_then(serde_json::Value::as_str)
    }
}

/// A `job_items` row, the JSON columns parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct JobItem {
    pub id: i64,
    pub job_id: i64,
    pub seq: i64,
    pub phase: i64,
    pub action: String,
    pub params: serde_json::Value,
    pub status: JobItemStatus,
    pub api_result: Option<serde_json::Value>,
    pub inverse: Option<serde_json::Value>,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A job to insert, with its first phase's items. Later phases are appended with
/// [`repo::add_items`] once the barrier before them holds.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub account_id: Option<String>,
    pub kind: JobKind,
    pub params: serde_json::Value,
    pub priority: i64,
    pub total_phases: i64,
    pub est_units_total: i64,
    pub items: Vec<NewJobItem>,
}

#[derive(Debug, Clone)]
pub struct NewJobItem {
    pub phase: i64,
    pub action: String,
    pub params: serde_json::Value,
}

/// Which jobs [`repo::list_jobs`] returns. The default is every job, newest first.
#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    pub account_id: Option<String>,
    pub statuses: Option<Vec<JobStatus>>,
    pub limit: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_statuses_round_trip() {
        for kind in [
            JobKind::CopyItems,
            JobKind::MoveItems,
            JobKind::RemoveItems,
            JobKind::Reorder,
            JobKind::CreatePlaylist,
            JobKind::RenamePlaylist,
        ] {
            assert_eq!(JobKind::parse(kind.as_str()), kind);
        }
        assert_eq!(JobKind::parse("split_by_channel"), JobKind::Other("split_by_channel".into()));
        for status in JobStatus::ALL {
            assert_eq!(JobStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(JobStatus::parse("bogus"), None);
        for status in JobItemStatus::ALL {
            assert_eq!(JobItemStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(JobItemStatus::parse("bogus"), None);
        let terminal: Vec<_> = JobStatus::ALL.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(
            terminal,
            [
                JobStatus::Completed,
                JobStatus::CompletedWithErrors,
                JobStatus::Failed,
                JobStatus::Cancelled
            ]
        );
    }
}
