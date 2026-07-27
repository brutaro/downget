use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use futures_util::{stream::FuturesUnordered, StreamExt};
use reqwest::{
    header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE},
    Client, StatusCode,
};
use tokio_util::sync::CancellationToken;

use crate::{
    model::{Job, Segment, TransferMode},
    part_file::PartFile,
    retry::{self, RetryClass, MAX_ATTEMPTS},
    source::{self, parse_content_range},
    store::Store,
    ui, Error, Result,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    Completed,
    Paused,
    AwaitingUrl { one_drive_public: bool },
    RequiresReinspect,
    Failed(String),
}

#[derive(Clone)]
struct RunContext<'a> {
    client: &'a Client,
    store: &'a Store,
    job: &'a Job,
    url: &'a str,
    ephemeral: bool,
    cancel: CancellationToken,
    progress: ui::Progress,
}

pub async fn run(
    client: &Client,
    store: &Store,
    job: &Job,
    url: &str,
    ephemeral: bool,
    is_resume: bool,
    cancel: CancellationToken,
) -> Result<TransferOutcome> {
    let part = PartFile::open(&job.part_path)?;
    let progress = ui::Progress::new(
        job.dest_path.display().to_string(),
        job.size,
        job.effective_concurrency.max(1),
        matches!(job.transfer_mode, Some(TransferMode::Segmented)),
    );
    let context = RunContext {
        client,
        store,
        job,
        url,
        ephemeral,
        cancel,
        progress,
    };
    match job
        .transfer_mode
        .ok_or_else(|| Error::Internal("Job sem modo de transferência".into()))?
    {
        TransferMode::Simple => run_simple(context, &part, is_resume).await,
        TransferMode::Segmented => run_segmented(context, &part).await,
    }
}

async fn run_simple(
    context: RunContext<'_>,
    _part: &PartFile,
    is_resume: bool,
) -> Result<TransferOutcome> {
    let RunContext {
        client,
        store,
        job,
        url,
        ephemeral,
        cancel,
        progress,
    } = context;
    if is_resume {
        eprintln!("Fonte sem Range: retomada por bytes não é possível; descartando o parcial com segurança e reiniciando do byte zero.");
    } else {
        eprintln!("Fonte sem Range: a retomada por blocos não está disponível.");
    }
    'attempts: loop {
        if cancel.is_cancelled() {
            return Ok(TransferOutcome::Paused);
        }
        let Some(attempt) = store.begin_simple_attempt(job.id)? else {
            return Ok(TransferOutcome::Failed(
                "orçamento de retry esgotado (5 tentativas persistidas)".into(),
            ));
        };
        if store.pause_requested(job.id)? {
            return Ok(TransferOutcome::Paused);
        }
        // A simple transfer can never safely append a retry; each attempt is
        // a clean transfer from byte zero.
        let part = PartFile::reset(&job.part_path)?;
        let response = match tokio::select! {
            _ = cancel.cancelled() => return Ok(TransferOutcome::Paused),
            response = client.get(url).send() => response,
        } {
            Ok(response) => response,
            Err(_) => {
                if attempt == MAX_ATTEMPTS {
                    return Ok(TransferOutcome::Failed("rede".into()));
                }
                wait_cancellable(store, job.id, retry::delay(attempt, None)).await?;
                continue;
            }
        };
        let status = response.status();
        let retry_after = retry::retry_after_header(response.headers());
        if status.is_success() {
            if let Some(item) = source::onedrive_item_from_download_url(url) {
                if let Some(message) = source::onedrive_delivery_error(item, response.headers()) {
                    return Ok(TransferOutcome::Failed(message.into()));
                }
            } else if source::is_html_content_type(response.headers()) {
                return Ok(TransferOutcome::Failed(
                    source::html_landing_page_message().into(),
                ));
            }
            let mut response = response;
            loop {
                let chunk = match tokio::select! {
                    _ = cancel.cancelled() => return Ok(TransferOutcome::Paused),
                    chunk = response.chunk() => chunk,
                } {
                    Ok(chunk) => chunk,
                    Err(_) => {
                        part.sync()?;
                        if attempt == MAX_ATTEMPTS {
                            return Ok(TransferOutcome::Failed(
                                "rede; orçamento de retry esgotado".into(),
                            ));
                        }
                        wait_cancellable(store, job.id, retry::delay(attempt, None)).await?;
                        continue 'attempts;
                    }
                };
                let Some(chunk) = chunk else { break };
                if store.pause_requested(job.id)? {
                    part.sync()?;
                    return Ok(TransferOutcome::Paused);
                }
                part.append(&chunk)?;
                progress.advance(chunk.len() as u64, attempt);
            }
            part.sync()?;
            return Ok(TransferOutcome::Completed);
        }
        match retry::classify_status(status, false, ephemeral) {
            RetryClass::Forbidden => {
                return Ok(TransferOutcome::AwaitingUrl {
                    one_drive_public: source::is_onedrive_download_url(url),
                })
            }
            RetryClass::Terminal | RetryClass::RequiresReinspect => {
                return Ok(TransferOutcome::Failed(format!("HTTP {}", status.as_u16())))
            }
            RetryClass::Retryable => {
                if attempt == MAX_ATTEMPTS {
                    return Ok(TransferOutcome::Failed(format!("HTTP {}", status.as_u16())));
                }
                wait_cancellable(store, job.id, retry::delay(attempt, retry_after.as_deref()))
                    .await?;
                continue;
            }
        }
    }
}

async fn run_segmented(context: RunContext<'_>, part: &PartFile) -> Result<TransferOutcome> {
    let RunContext {
        client,
        store,
        job,
        url,
        ephemeral,
        cancel,
        progress,
    } = context;
    let total = job
        .size
        .ok_or_else(|| Error::Internal("modo segmentado sem tamanho".into()))?;
    let segments = store.segments(job.id)?;
    let mut queue: VecDeque<_> = segments
        .into_iter()
        .filter(|segment| !segment.complete())
        .collect();
    let mut workers = FuturesUnordered::new();
    let mut limit = usize::from(job.effective_concurrency.max(1));
    let mut terminal: Option<TransferOutcome> = None;
    // A worker marks this before it cancels the shared token.  Without this
    // durable in-memory reason, the supervisor could observe a concurrently
    // completed worker first and mistake the cancellation for a user pause.
    let requires_reinspect = Arc::new(AtomicBool::new(false));
    loop {
        if requires_reinspect.load(Ordering::SeqCst) {
            set_terminal(&mut terminal, TransferOutcome::RequiresReinspect, &cancel);
        }
        while terminal.is_none() && workers.len() < limit && !cancel.is_cancelled() {
            let Some(segment) = queue.pop_front() else {
                break;
            };
            workers.push(segment_worker(
                SegmentContext {
                    client: client.clone(),
                    store: store.clone(),
                    job_id: job.id,
                    url: url.to_owned(),
                    ephemeral,
                    part: part.clone(),
                    total,
                    allow_parallelism_reduction: limit > 1,
                    cancel: cancel.clone(),
                    progress: progress.clone(),
                    requires_reinspect: Arc::clone(&requires_reinspect),
                },
                segment,
            ));
        }
        let Some(result) = workers.next().await else {
            break;
        };
        if requires_reinspect.load(Ordering::SeqCst) {
            set_terminal(&mut terminal, TransferOutcome::RequiresReinspect, &cancel);
        } else {
            match result? {
                SegmentOutcome::Completed | SegmentOutcome::Cancelled => {}
                SegmentOutcome::Paused => {
                    set_terminal(&mut terminal, TransferOutcome::Paused, &cancel)
                }
                SegmentOutcome::AwaitingUrl { one_drive_public } => set_terminal(
                    &mut terminal,
                    TransferOutcome::AwaitingUrl { one_drive_public },
                    &cancel,
                ),
                SegmentOutcome::RequiresReinspect => {
                    set_terminal(&mut terminal, TransferOutcome::RequiresReinspect, &cancel)
                }
                SegmentOutcome::Failed(message) => {
                    set_terminal(&mut terminal, TransferOutcome::Failed(message), &cancel)
                }
                SegmentOutcome::ReduceConcurrency(segment, delay) => {
                    store.set_parallelism_reduced(job.id)?;
                    limit = 1;
                    if wait_worker(store, job.id, delay, &cancel).await? {
                        queue.push_front(segment);
                    }
                }
            }
        }
        if requires_reinspect.load(Ordering::SeqCst) {
            set_terminal(&mut terminal, TransferOutcome::RequiresReinspect, &cancel);
        } else if store.pause_requested(job.id)? || cancel.is_cancelled() {
            set_terminal(&mut terminal, TransferOutcome::Paused, &cancel);
        }
    }
    if let Some(outcome) = terminal {
        return Ok(outcome);
    }
    if !queue.is_empty() {
        return Ok(TransferOutcome::Paused);
    }
    if store
        .segments(job.id)?
        .iter()
        .any(|segment| !segment.complete())
    {
        return Ok(TransferOutcome::Failed(
            "há segmentos sem checkpoint durável".into(),
        ));
    }
    Ok(TransferOutcome::Completed)
}

#[derive(Debug)]
enum SegmentOutcome {
    Completed,
    Cancelled,
    Paused,
    AwaitingUrl { one_drive_public: bool },
    RequiresReinspect,
    Failed(String),
    ReduceConcurrency(Segment, Duration),
}

#[derive(Clone)]
struct SegmentContext {
    client: Client,
    store: Store,
    job_id: i64,
    url: String,
    ephemeral: bool,
    part: PartFile,
    total: u64,
    allow_parallelism_reduction: bool,
    cancel: CancellationToken,
    progress: ui::Progress,
    requires_reinspect: Arc<AtomicBool>,
}

/// Couples the file durability boundary to the SQLite checkpoint when a
/// running segmented transfer is asked to pause.  Callers pass the exclusive
/// end of bytes already written, so this can never advertise an unwritten
/// byte to a future `resume`.
struct PauseCheckpoint<'a> {
    store: &'a Store,
    part: &'a PartFile,
    job_id: i64,
    segment: &'a Segment,
    request_start: u64,
    attempt: u8,
}

impl PauseCheckpoint<'_> {
    fn persist_written_prefix(&self, offset: u64, checkpointed_end: u64) -> Result<()> {
        self.part.sync()?;
        if offset > self.request_start && checkpointed_end < offset - 1 {
            self.store.checkpoint_segment_at(
                self.job_id,
                self.segment,
                offset - 1,
                self.attempt,
            )?;
        }
        Ok(())
    }
}

fn set_terminal(
    slot: &mut Option<TransferOutcome>,
    outcome: TransferOutcome,
    cancel: &CancellationToken,
) {
    if slot.is_none() {
        *slot = Some(outcome);
        cancel.cancel();
    }
}

async fn segment_worker(context: SegmentContext, segment: Segment) -> Result<SegmentOutcome> {
    let SegmentContext {
        client,
        store,
        job_id,
        url,
        ephemeral,
        part,
        total,
        allow_parallelism_reduction,
        cancel,
        progress,
        requires_reinspect,
    } = context;
    let request_start = (segment.committed_end + 1) as u64;
    if request_start > segment.end {
        return Ok(SegmentOutcome::Completed);
    }
    let expected_len = segment.end - request_start + 1;
    'attempts: loop {
        if cancel.is_cancelled() {
            return Ok(SegmentOutcome::Cancelled);
        }
        let Some(attempt) = store.begin_segment_attempt(job_id, segment.ordinal)? else {
            store.fail_segment_terminal(job_id, segment.ordinal, "retry_exhausted")?;
            return Ok(SegmentOutcome::Failed(
                "orçamento de retry esgotado (5 tentativas persistidas)".into(),
            ));
        };
        if store.pause_requested(job_id)? {
            return Ok(SegmentOutcome::Paused);
        }
        let response = match tokio::select! {
            _ = cancel.cancelled() => return Ok(SegmentOutcome::Cancelled),
            response = client.get(&url).header(RANGE, format!("bytes={request_start}-{}", segment.end)).send() => response,
        } {
            Ok(response) => response,
            Err(_) => {
                if attempt == MAX_ATTEMPTS {
                    store.fail_segment_terminal(
                        job_id,
                        segment.ordinal,
                        "network_retry_exhausted",
                    )?;
                    return Ok(SegmentOutcome::Failed(
                        "rede; orçamento de retry esgotado".into(),
                    ));
                }
                if !wait_worker(&store, job_id, retry::delay(attempt, None), &cancel).await? {
                    return Ok(SegmentOutcome::Cancelled);
                }
                continue;
            }
        };
        let status = response.status();
        let retry_after = retry::retry_after_header(response.headers());
        if status.is_success() {
            if let Some(item) = source::onedrive_item_from_download_url(&url) {
                if let Some(message) = source::onedrive_delivery_error(item, response.headers()) {
                    return Ok(SegmentOutcome::Failed(message.into()));
                }
            } else if source::is_html_content_type(response.headers()) {
                return Ok(SegmentOutcome::Failed(
                    source::html_landing_page_message().into(),
                ));
            }
        }
        if allow_parallelism_reduction
            && (status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::SERVICE_UNAVAILABLE)
        {
            // The supervisor stops launching parallel work after this outcome.
            // The segment itself is retried by a later resume rather than
            // accepting an ambiguous response.
            return Ok(SegmentOutcome::ReduceConcurrency(
                segment,
                retry::delay(attempt, retry_after.as_deref()),
            ));
        }
        if status != StatusCode::PARTIAL_CONTENT {
            match retry::classify_status(status, true, ephemeral) {
                RetryClass::Forbidden => {
                    return Ok(SegmentOutcome::AwaitingUrl {
                        one_drive_public: source::is_onedrive_download_url(&url),
                    })
                }
                RetryClass::RequiresReinspect => {
                    requires_reinspect.store(true, Ordering::SeqCst);
                    cancel.cancel();
                    return Ok(SegmentOutcome::RequiresReinspect);
                }
                RetryClass::Terminal => {
                    return Ok(SegmentOutcome::Failed(format!("HTTP {}", status.as_u16())))
                }
                RetryClass::Retryable => {
                    if attempt == MAX_ATTEMPTS {
                        store.fail_segment_terminal(
                            job_id,
                            segment.ordinal,
                            "http_retry_exhausted",
                        )?;
                        return Ok(SegmentOutcome::Failed(format!(
                            "HTTP {}; orçamento de retry esgotado",
                            status.as_u16()
                        )));
                    }
                    if !wait_worker(
                        &store,
                        job_id,
                        retry::delay(attempt, retry_after.as_deref()),
                        &cancel,
                    )
                    .await?
                    {
                        return Ok(SegmentOutcome::Cancelled);
                    }
                    continue;
                }
            }
        }
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range);
        let length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if content_range != Some((request_start, segment.end, Some(total)))
            || length.is_some_and(|value| value != expected_len)
        {
            requires_reinspect.store(true, Ordering::SeqCst);
            cancel.cancel();
            return Ok(SegmentOutcome::RequiresReinspect);
        }
        let mut response = response;
        let mut offset = request_start;
        let mut checkpointed_end = request_start.saturating_sub(1);
        let mut bytes_since_checkpoint = 0_u64;
        let mut checkpoint_at = tokio::time::Instant::now();
        let pause_checkpoint = PauseCheckpoint {
            store: &store,
            part: &part,
            job_id,
            segment: &segment,
            request_start,
            attempt,
        };
        loop {
            let chunk = match tokio::select! {
                _ = cancel.cancelled() => {
                    if store.pause_requested(job_id)? {
                        pause_checkpoint.persist_written_prefix(offset, checkpointed_end)?;
                        return Ok(SegmentOutcome::Paused);
                    }
                    return Ok(SegmentOutcome::Cancelled);
                },
                chunk = response.chunk() => chunk,
            } {
                Ok(chunk) => chunk,
                Err(_) => {
                    if attempt == MAX_ATTEMPTS {
                        store.fail_segment_terminal(
                            job_id,
                            segment.ordinal,
                            "stream_retry_exhausted",
                        )?;
                        return Ok(SegmentOutcome::Failed(
                            "rede; orçamento de retry esgotado".into(),
                        ));
                    }
                    if !wait_worker(&store, job_id, retry::delay(attempt, None), &cancel).await? {
                        if store.pause_requested(job_id)? {
                            pause_checkpoint.persist_written_prefix(offset, checkpointed_end)?;
                            return Ok(SegmentOutcome::Paused);
                        }
                        return Ok(SegmentOutcome::Cancelled);
                    }
                    continue 'attempts;
                }
            };
            let Some(chunk) = chunk else {
                // `response.chunk()` and the cancellation token can become
                // ready together at EOF.  Prefer the durable pause protocol
                // when the control row is already present rather than
                // promoting a final file after the user pressed Ctrl+C.
                if store.pause_requested(job_id)? {
                    pause_checkpoint.persist_written_prefix(offset, checkpointed_end)?;
                    return Ok(SegmentOutcome::Paused);
                }
                break;
            };
            if store.pause_requested(job_id)? {
                pause_checkpoint.persist_written_prefix(offset, checkpointed_end)?;
                return Ok(SegmentOutcome::Paused);
            }
            if cancel.is_cancelled() {
                if store.pause_requested(job_id)? {
                    pause_checkpoint.persist_written_prefix(offset, checkpointed_end)?;
                    return Ok(SegmentOutcome::Paused);
                }
                return Ok(SegmentOutcome::Cancelled);
            }
            if offset.saturating_add(chunk.len() as u64) > segment.end + 1 {
                requires_reinspect.store(true, Ordering::SeqCst);
                cancel.cancel();
                return Ok(SegmentOutcome::RequiresReinspect);
            }
            part.write_at(offset, &chunk)?;
            offset += chunk.len() as u64;
            progress.advance(chunk.len() as u64, attempt);
            bytes_since_checkpoint += chunk.len() as u64;
            if bytes_since_checkpoint >= 8 * 1024 * 1024
                || checkpoint_at.elapsed() >= Duration::from_secs(1)
            {
                part.sync()?;
                checkpointed_end = offset - 1;
                store.checkpoint_segment_at(job_id, &segment, checkpointed_end, attempt)?;
                bytes_since_checkpoint = 0;
                checkpoint_at = tokio::time::Instant::now();
            }
        }
        if store.pause_requested(job_id)? {
            pause_checkpoint.persist_written_prefix(offset, checkpointed_end)?;
            return Ok(SegmentOutcome::Paused);
        }
        if offset != segment.end + 1 {
            requires_reinspect.store(true, Ordering::SeqCst);
            cancel.cancel();
            return Ok(SegmentOutcome::RequiresReinspect);
        }
        // This is the durability boundary: no SQLite checkpoint is made until
        // every written byte has been pushed to the partial file.
        part.sync()?;
        if checkpointed_end != segment.end {
            store.checkpoint_segment(job_id, &segment, attempt)?;
        }
        return Ok(SegmentOutcome::Completed);
    }
}

async fn wait_cancellable(store: &Store, job_id: i64, delay: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if store.pause_requested(job_id)? {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(());
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(200))).await;
    }
}

async fn wait_worker(
    store: &Store,
    job_id: i64,
    delay: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    tokio::select! {
        _ = cancel.cancelled() => Ok(false),
        result = wait_cancellable(store, job_id, delay) => {
            result?;
            Ok(!cancel.is_cancelled() && !store.pause_requested(job_id)?)
        }
    }
}
