use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use url::Url;

#[derive(Clone)]
pub struct Progress {
    name: String,
    total: Option<u64>,
    connections: u8,
    resumable: bool,
    started: Instant,
    completed: Arc<AtomicU64>,
    last_render: Arc<Mutex<Instant>>,
}

impl Progress {
    pub fn new(name: String, total: Option<u64>, connections: u8, resumable: bool) -> Self {
        let started = Instant::now();
        Self {
            name,
            total,
            connections,
            resumable,
            started,
            completed: Arc::new(AtomicU64::new(0)),
            // Permit an immediate first line, then keep the terminal usable
            // while high-throughput responses stream many small chunks.
            last_render: Arc::new(Mutex::new(started - Duration::from_secs(1))),
        }
    }

    pub fn advance(&self, bytes: u64, attempt: u8) {
        let completed = self.completed.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let mut last_render = self.last_render.lock().expect("progress lock");
        if last_render.elapsed() < Duration::from_millis(200) {
            return;
        }
        *last_render = Instant::now();
        let seconds = self.started.elapsed().as_secs_f64().max(0.001);
        let speed = completed as f64 / seconds;
        let percentage = self
            .total
            .filter(|total| *total > 0)
            .map(|total| completed as f64 * 100.0 / total as f64);
        let eta = self.total.and_then(|total| {
            (speed > 0.0).then(|| (total.saturating_sub(completed) as f64 / speed).ceil() as u64)
        });
        eprintln!("{} {}  {completed} / {} bytes | {:.1} KiB/s | ETA {} | {}/{} conexões | tentativa {attempt} | {}",
            self.name,
            percentage.map(|value| format!("{value:5.1}%")).unwrap_or_else(|| "  ?.?%".into()),
            self.total.map(|value| value.to_string()).unwrap_or_else(|| "?".into()),
            speed / 1024.0,
            eta.map(|value| format!("{value}s")).unwrap_or_else(|| "?".into()),
            self.connections, self.connections,
            if self.resumable { "retomável" } else { "sem retomada por blocos" });
    }
}

/// Displays no userinfo, query or fragment, regardless of whether a source is
/// retained in state.  Use this at every user-visible HTTP boundary.
pub fn redact_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return "[fonte redigida]".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string().trim_end_matches('/').to_owned()
}

pub fn source_display(raw: &str) -> String {
    redact_url(raw)
}

pub fn progress(name: &str, completed: u64, total: Option<u64>, connections: u8, resumable: bool) {
    let percentage = total
        .filter(|total| *total > 0)
        .map(|total| completed as f64 * 100.0 / total as f64);
    let percentage = percentage
        .map(|value| format!("{value:5.1}%"))
        .unwrap_or_else(|| "  ?.?%".into());
    let total = total
        .map(|value| value.to_string())
        .unwrap_or_else(|| "?".into());
    eprintln!(
        "{name} {percentage}  {completed} / {total} bytes | {connections} conexão(ões) | {}",
        if resumable {
            "retomável"
        } else {
            "sem retomada por blocos"
        }
    );
}

pub fn next_action(job_id: i64, state: &str, requires_url: bool) -> String {
    if requires_url {
        return format!("use `downget resume {job_id} --url <NOVA_URL>`");
    }
    match state {
        "paused" | "failed_terminal" | "requires_reinspect" => {
            format!("use `downget resume {job_id}`")
        }
        "completed" => "concluído".into(),
        _ => "em andamento".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn redaction_removes_userinfo_query_and_fragment() {
        let display = redact_url("https://user:token@example.test/a?signature=sentinel#fragment");
        assert_eq!(display, "https://example.test/a");
        assert!(!display.contains("sentinel"));
    }
}
