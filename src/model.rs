use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Initializing,
    Probing,
    RunningSimple,
    RunningSegmented,
    Paused,
    RequiresReinspect,
    FailedTerminal,
    Finalizing,
    Completed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initializing => "initializing",
            Self::Probing => "probing",
            Self::RunningSimple => "running_simple",
            Self::RunningSegmented => "running_segmented",
            Self::Paused => "paused",
            Self::RequiresReinspect => "requires_reinspect",
            Self::FailedTerminal => "failed_terminal",
            Self::Finalizing => "finalizing",
            Self::Completed => "completed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "initializing" => Self::Initializing,
            "probing" => Self::Probing,
            "running_simple" => Self::RunningSimple,
            "running_segmented" => Self::RunningSegmented,
            "paused" => Self::Paused,
            "requires_reinspect" => Self::RequiresReinspect,
            "failed_terminal" => Self::FailedTerminal,
            "finalizing" => Self::Finalizing,
            "completed" => Self::Completed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    Simple,
    Segmented,
}

impl TransferMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Segmented => "segmented",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "simple" => Some(Self::Simple),
            "segmented" => Some(Self::Segmented),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlMode {
    Retained,
    ReplacementRequired,
}

impl UrlMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Retained => "retained",
            Self::ReplacementRequired => "replacement_required",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "retained" => Some(Self::Retained),
            "replacement_required" => Some(Self::ReplacementRequired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub dest_path: PathBuf,
    pub part_path: PathBuf,
    pub state: JobState,
    pub transfer_mode: Option<TransferMode>,
    pub requested_concurrency: u8,
    pub effective_concurrency: u8,
    pub parallelism_note: Option<String>,
    pub url_mode: UrlMode,
    pub safe_url: Option<String>,
    pub source_display: String,
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub sha256_expected: Option<String>,
    pub retry_summary: Option<String>,
    pub last_error_code: Option<String>,
    pub last_error_action: Option<String>,
    pub pause_requested: bool,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub ordinal: u32,
    pub start: u64,
    pub end: u64,
    /// The last durably checkpointed byte. `start - 1` denotes no bytes.
    pub committed_end: i64,
    pub attempts_used: u8,
    pub state: String,
}

impl Segment {
    pub fn complete(&self) -> bool {
        self.committed_end >= self.end as i64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl SourceIdentity {
    pub fn matches(&self, other: &Self) -> bool {
        match (&self.etag, &other.etag) {
            (Some(left), Some(right)) if is_strong_etag(left) && is_strong_etag(right) => {
                left == right
            }
            _ => match (
                self.size,
                other.size,
                &self.last_modified,
                &other.last_modified,
            ) {
                (Some(left_size), Some(right_size), Some(left_time), Some(right_time)) => {
                    left_size == right_size && left_time == right_time
                }
                _ => false,
            },
        }
    }
}

pub fn is_strong_etag(value: &str) -> bool {
    !value.trim_start().starts_with("W/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_requires_strong_etag_or_size_and_date() {
        let base = SourceIdentity {
            size: Some(10),
            etag: Some("\"v1\"".into()),
            last_modified: None,
        };
        assert!(base.matches(&base));
        let changed = SourceIdentity {
            etag: Some("\"v2\"".into()),
            ..base.clone()
        };
        assert!(!base.matches(&changed));
        let weak = SourceIdentity {
            size: Some(10),
            etag: Some("W/\"v1\"".into()),
            last_modified: None,
        };
        assert!(!weak.matches(&weak));
        let dated = SourceIdentity {
            size: Some(10),
            etag: None,
            last_modified: Some("Tue, 01 Jan 2030 00:00:00 GMT".into()),
        };
        assert!(dated.matches(&dated));
    }
}
