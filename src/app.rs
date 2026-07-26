use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    cli::{Cli, Command, ConfigCommand},
    lease::JobLease,
    model::{Job, JobState, SourceIdentity, TransferMode, UrlMode},
    part_file::{sync_directory, PartFile},
    source::{self, ProbeMode},
    store::Store,
    transfer::{self, TransferOutcome},
    ui, Error, Result,
};

pub struct App {
    store: Store,
    client: reqwest::Client,
}

impl App {
    pub fn new() -> Result<Self> {
        Ok(Self {
            store: Store::open_default()?,
            client: source::client()?,
        })
    }

    pub async fn run(&self, cli: Cli) -> Result<()> {
        match cli.command {
            Command::Add {
                url,
                output,
                sha256,
            } => self.add(&url, output.as_deref(), sha256.as_deref()).await,
            Command::List => self.list(),
            Command::Resume { id, url, sha256 } => {
                self.resume(id, url.as_deref(), sha256.as_deref()).await
            }
            Command::Cancel { id, discard } => self.cancel(id, discard).await,
            Command::Config {
                command: ConfigCommand::Set { key: _, value },
            } => {
                self.store.set_concurrency(value)?;
                println!("Concorrência global definida para {value}.");
                Ok(())
            }
        }
    }

    async fn add(&self, raw_url: &str, output: Option<&Path>, sha256: Option<&str>) -> Result<()> {
        // Validate before making a persistent row, but do not retain the URL.
        source::validate_url(raw_url)?;
        let concurrency = self.store.get_concurrency()?;
        let id = self.store.create_initial_job(concurrency, sha256)?;
        // From this point a concurrent `cancel` can only write a durable pause
        // request; it cannot race initialization and later be overwritten.
        let _lease = JobLease::acquire(&self.store, id)?;
        let probe = match source::probe(&self.client, raw_url).await {
            Ok(probe) => probe,
            Err(error) => {
                self.store.delete_initial_job(id)?;
                return Err(error);
            }
        };
        let destination = resolve_destination(output, probe.filename.as_deref(), id)?;
        let part_path = part_path(&destination);
        if destination.exists() || part_path.exists() {
            self.store.delete_initial_job(id)?;
            return Err(Error::User(format!(
                "destino já existe: {}",
                destination.display()
            )));
        }
        let mode = transfer_mode(probe.mode);
        let url_mode = if probe.ephemeral {
            UrlMode::ReplacementRequired
        } else {
            UrlMode::Retained
        };
        self.store.initialize_job(
            id,
            &destination,
            &part_path,
            mode,
            probe.identity.size,
            probe.identity.etag.as_deref(),
            probe.identity.last_modified.as_deref(),
            url_mode,
            (!probe.ephemeral).then_some(probe.url.as_str()),
            &probe.source_display,
        )?;
        let _part = match PartFile::create_new(
            &part_path,
            (mode == TransferMode::Segmented)
                .then_some(probe.identity.size)
                .flatten(),
        ) {
            Ok(part) => part,
            Err(error) => {
                self.store.delete_initial_job(id)?;
                return Err(error);
            }
        };
        if mode == TransferMode::Segmented {
            let ranges = plan_segments(
                probe.identity.size.expect("Range proof has a total"),
                concurrency,
            );
            if let Err(error) = self.store.create_segments(id, &ranges) {
                let _ = fs::remove_file(&part_path);
                self.store.delete_initial_job(id)?;
                return Err(error);
            }
        }
        println!("Job {id} criado: {}", destination.display());
        let job = self.store.job(id)?;
        if self.store.pause_requested(id)? {
            self.store.set_state(
                id,
                JobState::Paused,
                Some("cancelled_during_probe"),
                Some("use resume para continuar"),
            )?;
            self.store.acknowledge_pause(id)?;
            return Ok(());
        }
        self.execute(job, probe.url, probe.ephemeral, false).await
    }

    async fn resume(
        &self,
        id: i64,
        supplied_url: Option<&str>,
        sha256: Option<&str>,
    ) -> Result<()> {
        // The lease is deliberately first: checksum validation is a mutation
        // and must not race a process that is finalizing this same Job.
        let _lease = JobLease::acquire(&self.store, id)?;
        let mut job = self.store.job(id)?;
        if job.state == JobState::Completed {
            return Err(Error::User(format!("Job {id} já foi concluído")));
        }
        validate_or_store_checksum(&self.store, &job, sha256)?;
        job = self.store.job(id)?;
        if job.url_mode == UrlMode::ReplacementRequired && supplied_url.is_none() {
            return Err(Error::User(format!(
                "Job {id} requer uma nova URL. Use `downget resume {id} --url <NOVA_URL>`"
            )));
        }
        let url = supplied_url
            .map(str::to_owned)
            .or(job.safe_url.clone())
            .ok_or_else(|| {
                Error::User(format!(
                    "Job {id} não tem uma URL reutilizável; use `--url <NOVA_URL>`"
                ))
            })?;
        let probe = source::probe(&self.client, &url).await?;
        let stored_identity = SourceIdentity {
            size: job.size,
            etag: job.etag.clone(),
            last_modified: job.last_modified.clone(),
        };
        if !stored_identity.matches(&probe.identity) {
            return Err(Error::User(
                "a identidade da fonte mudou ou não pode ser comprovada; o parcial foi preservado"
                    .into(),
            ));
        }
        let mode = transfer_mode(probe.mode);
        if job.transfer_mode != Some(mode) {
            if mode != TransferMode::Simple {
                return Err(Error::User(
                    "a fonte mudou de estratégia de Range; inicie outro Job".into(),
                ));
            }
            eprintln!(
                "A fonte não permite mais Range; descartando parcial e reiniciando do byte zero."
            );
            PartFile::reset(&job.part_path)?;
            self.store.reset_segments(id)?;
        }
        self.store.refresh_source(
            id,
            mode,
            probe.identity.size,
            probe.identity.etag.as_deref(),
            probe.identity.last_modified.as_deref(),
            &probe.source_display,
        )?;
        if probe.ephemeral {
            self.store.require_replacement_url(id)?;
        }
        job = self.store.job(id)?;
        self.execute(job, probe.url, probe.ephemeral, true).await
    }

    async fn execute(&self, job: Job, url: String, ephemeral: bool, is_resume: bool) -> Result<()> {
        let state = match job.transfer_mode {
            Some(TransferMode::Simple) => JobState::RunningSimple,
            Some(TransferMode::Segmented) => JobState::RunningSegmented,
            None => return Err(Error::Internal("Job sem transferência".into())),
        };
        self.store.set_state(job.id, state, None, None)?;
        self.store.set_active(job.id, true)?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut transfer = Box::pin(transfer::run(
            &self.client,
            &self.store,
            &job,
            &url,
            ephemeral,
            is_resume,
            cancellation.clone(),
        ));
        let (transfer_result, interrupted) = tokio::select! {
            outcome = &mut transfer => (outcome, false),
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|_| Error::Internal("não foi possível observar Ctrl+C".into()))?;
                self.store.request_pause(job.id)?;
                cancellation.cancel();
                (transfer.await, true)
            }
        };
        self.store.set_active(job.id, false)?;
        let outcome = match transfer_result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.store.set_state(
                    job.id,
                    JobState::FailedTerminal,
                    Some("transfer_error"),
                    Some("use resume para tentar novamente"),
                )?;
                return Err(error);
            }
        };
        match outcome {
            TransferOutcome::Completed => {
                self.store
                    .set_state(job.id, JobState::Finalizing, None, None)?;
                let part = PartFile::open(&job.part_path)?;
                match part.finalize(&job.dest_path, job.size, job.sha256_expected.as_deref()) {
                    Ok(()) => {
                        self.store
                            .set_state(job.id, JobState::Completed, None, None)?;
                        println!("Concluído: {}", job.dest_path.display());
                        Ok(())
                    }
                    Err(error) => {
                        self.store.set_state(
                            job.id,
                            JobState::FailedTerminal,
                            Some("validation_failed"),
                            Some("verifique o parcial ou descarte explicitamente"),
                        )?;
                        Err(error)
                    }
                }
            }
            TransferOutcome::Paused => {
                self.store.set_state(
                    job.id,
                    JobState::Paused,
                    Some("paused"),
                    Some("use resume para continuar"),
                )?;
                self.store.acknowledge_pause(job.id)?;
                eprintln!(
                    "Pausado com segurança. Use `downget resume {}` para continuar.",
                    job.id
                );
                if interrupted {
                    Err(Error::Interrupted)
                } else {
                    Ok(())
                }
            }
            TransferOutcome::AwaitingUrl => {
                self.store.require_replacement_url(job.id)?;
                self.store.set_state(
                    job.id,
                    JobState::Paused,
                    Some("awaiting_url"),
                    Some("forneça uma nova URL"),
                )?;
                Err(Error::User(format!(
                    "a fonte retornou 403. Use `downget resume {} --url <NOVA_URL>`",
                    job.id
                )))
            }
            TransferOutcome::RequiresReinspect => {
                self.store.set_state(
                    job.id,
                    JobState::RequiresReinspect,
                    Some("range_protocol"),
                    Some("inspecione novamente com resume"),
                )?;
                Err(Error::User(format!(
                    "a resposta Range foi insegura. Use `downget resume {}`",
                    job.id
                )))
            }
            TransferOutcome::Failed(reason) => {
                self.store.set_state(
                    job.id,
                    JobState::FailedTerminal,
                    Some("retry_or_transfer"),
                    Some("use resume para tentar novamente"),
                )?;
                Err(Error::User(format!(
                    "transferência falhou ({reason}); o parcial foi preservado"
                )))
            }
        }
    }

    fn list(&self) -> Result<()> {
        for job in self.store.jobs()? {
            let progress = match job.transfer_mode {
                Some(TransferMode::Segmented) => {
                    self.store
                        .segments(job.id)?
                        .iter()
                        .filter(|segment| segment.complete())
                        .count()
                        .to_string()
                        + " segmentos completos"
                }
                _ => "progresso simples disponível no parcial".into(),
            };
            println!(
                "{}  {}  {}  {}",
                job.id,
                job.dest_path.display(),
                job.state.as_str(),
                ui::next_action(
                    job.id,
                    job.state.as_str(),
                    job.url_mode == UrlMode::ReplacementRequired
                )
            );
            println!("    {progress}");
            if let Some(note) = job.parallelism_note {
                println!("    paralelismo: {note}");
            }
        }
        Ok(())
    }

    async fn cancel(&self, id: i64, discard: bool) -> Result<()> {
        let job = self.store.job(id)?;
        if job.state == JobState::Completed {
            return Err(Error::User(
                "Jobs concluídos não podem ser cancelados".into(),
            ));
        }
        let lease = match JobLease::acquire(&self.store, id) {
            Ok(lease) => lease,
            Err(_) => {
                let sequence = self.store.request_pause(id)?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    if self.store.pause_acknowledged(id, sequence)? {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Error::User("a parada não foi confirmada em 10 segundos; nenhum dado foi descartado".into()));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                JobLease::acquire(&self.store, id)?
            }
        };
        let _lease = lease;
        self.store.set_state(
            id,
            JobState::Paused,
            Some("cancelled"),
            Some("use resume para continuar"),
        )?;
        if !discard {
            println!("Job {id} pausado; parcial e estado foram preservados.");
            return Ok(());
        }
        // `--discard` is the explicit, irreversible confirmation.  It is only
        // reached while this process owns the lease.
        if job.part_path.exists() {
            fs::remove_file(&job.part_path)?;
            sync_directory(job.part_path.parent().unwrap_or_else(|| Path::new(".")))?;
        }
        self.store.delete_job(id)?;
        println!("Job {id} e seu arquivo parcial foram descartados permanentemente.");
        Ok(())
    }
}

fn transfer_mode(mode: ProbeMode) -> TransferMode {
    match mode {
        ProbeMode::Simple => TransferMode::Simple,
        ProbeMode::Segmented => TransferMode::Segmented,
    }
}

fn part_path(destination: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", destination.display()))
}

fn resolve_destination(output: Option<&Path>, filename: Option<&str>, id: i64) -> Result<PathBuf> {
    let name = filename
        .map(str::to_owned)
        .unwrap_or_else(|| format!("download-{id}"));
    let destination = match output {
        Some(output) if output.is_dir() => output.join(name),
        Some(output) => output.to_path_buf(),
        None => std::env::current_dir()?.join(name),
    };
    if destination.file_name().is_none() {
        return Err(Error::User(
            "--output precisa apontar para um arquivo ou diretório".into(),
        ));
    }
    Ok(destination)
}

fn validate_or_store_checksum(store: &Store, job: &Job, supplied: Option<&str>) -> Result<()> {
    match (job.sha256_expected.as_deref(), supplied) {
        (Some(expected), Some(value)) if expected != value => Err(Error::User(
            "o SHA-256 informado difere do valor já registrado".into(),
        )),
        (None, Some(value)) => store.update_sha256(job.id, value),
        _ => Ok(()),
    }
}

/// Static 16 MiB pieces keep the scheduler predictable.  The count never
/// exceeds the effective concurrency and no interval overlaps another.
fn plan_segments(total: u64, concurrency: u8) -> Vec<(u64, u64)> {
    const MIN_SEGMENT: u64 = 16 * 1024 * 1024;
    let count = u64::from(concurrency)
        .min(total.div_ceil(MIN_SEGMENT))
        .max(1);
    let base = total / count;
    let extra = total % count;
    let mut start = 0;
    (0..count)
        .map(|ordinal| {
            let len = base + u64::from(ordinal < extra);
            let range = (start, start + len - 1);
            start += len;
            range
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn planner_creates_contiguous_offsets() {
        let ranges = plan_segments(33 * 1024 * 1024, 2);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges[0].1 + 1, ranges[1].0);
        assert_eq!(ranges[1].1, 33 * 1024 * 1024 - 1);
    }

    #[tokio::test]
    async fn cancel_preserves_then_explicitly_discards_partial() {
        let root = std::env::temp_dir().join(format!(
            "downget-app-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(root.clone()).unwrap();
        let id = store.create_initial_job(2, None).unwrap();
        let destination = root.join("download");
        let partial = part_path(&destination);
        store
            .initialize_job(
                id,
                &destination,
                &partial,
                TransferMode::Simple,
                None,
                None,
                None,
                UrlMode::Retained,
                Some("http://example.test/file"),
                "http://example.test/file",
            )
            .unwrap();
        PartFile::create_new(&partial, None)
            .unwrap()
            .append(b"partial")
            .unwrap();
        let app = App {
            store: store.clone(),
            client: source::client().unwrap(),
        };

        app.cancel(id, false).await.unwrap();
        assert!(partial.exists());
        assert_eq!(store.job(id).unwrap().state, JobState::Paused);

        app.cancel(id, true).await.unwrap();
        assert!(!partial.exists());
        assert!(store.job(id).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn resume_does_not_mutate_sha256_while_another_process_holds_the_lease() {
        let root = std::env::temp_dir().join(format!(
            "downget-sha-lock-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(root.clone()).unwrap();
        let id = store.create_initial_job(2, None).unwrap();
        let destination = root.join("download");
        let partial = part_path(&destination);
        store
            .initialize_job(
                id,
                &destination,
                &partial,
                TransferMode::Simple,
                None,
                None,
                None,
                UrlMode::Retained,
                Some("http://example.test/file"),
                "http://example.test/file",
            )
            .unwrap();
        let _lease = JobLease::acquire(&store, id).unwrap();
        let app = App {
            store: store.clone(),
            client: source::client().unwrap(),
        };
        let checksum = "a".repeat(64);
        assert!(app.resume(id, None, Some(&checksum)).await.is_err());
        assert_eq!(store.job(id).unwrap().sha256_expected, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cross_process_pause_request_survives_resume_activation() {
        let root = std::env::temp_dir().join(format!(
            "downget-pause-race-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(root.clone()).unwrap();
        let id = store.create_initial_job(2, None).unwrap();
        // Simulates another CLI process writing the durable pause control while
        // this process is still probing the replacement source.
        let other_process = Store::open(root.clone()).unwrap();
        other_process.request_pause(id).unwrap();
        store.set_active(id, true).unwrap();
        assert!(store.pause_requested(id).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
