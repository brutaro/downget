use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use downget::{
    model::{JobState, TransferMode, UrlMode},
    part_file::PartFile,
    source,
    store::Store,
};

#[derive(Clone, Copy)]
enum RangeMode {
    Supported,
    Unsupported,
    InvalidProof,
    SimpleRetryOnce,
    ThrottleRangeOnce,
    Throttle503Once,
    ForbiddenAfterProof,
    SlowSupported,
    NoEtagSupported,
    NoEtagChangedLastModified,
    ChangedEtagSupported,
    MaliciousFilename,
    SensitiveResponseHeaders,
    AlwaysRetrySimple,
    ProbeThrottleOnce,
    Probe408Once,
    ProbeServerErrorOnce,
    ProbeNetworkFailureOnce,
    UnknownRangeTotal,
    WorkerReturns200,
    WorkerReturns416,
    WorkerInvalidContentRange,
}

struct Fixture {
    address: String,
    requests: Arc<Mutex<Vec<String>>>,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start(mode: RangeMode, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let thread_requests = Arc::clone(&requests);
        let thread_stopping = Arc::clone(&stopping);
        let thread_attempts = Arc::clone(&attempts);
        let thread = thread::spawn(move || {
            while !thread_stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        serve(stream, mode, &body, &thread_requests, &thread_attempts)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            requests,
            stopping,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/file", self.address)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    mode: RangeMode,
    body: &[u8],
    requests: &Mutex<Vec<String>>,
    attempts: &AtomicUsize,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(count) => request.extend_from_slice(&buffer[..count]),
            Err(_) => return,
        }
    }
    let request = String::from_utf8_lossy(&request);
    let range = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range")
            .then(|| value.trim().to_owned())
    });
    requests
        .lock()
        .unwrap()
        .push(range.clone().unwrap_or_else(|| "no-range".into()));

    // Closing the first proof request without a response exercises the same
    // transient-probe path that a connection timeout/reset would take.
    if matches!(mode, RangeMode::ProbeNetworkFailureOnce)
        && range.as_deref() == Some("bytes=0-0")
        && attempts.fetch_add(1, Ordering::Relaxed) == 0
    {
        return;
    }

    let (status, response_body, extra) = match (mode, range) {
        (RangeMode::ProbeThrottleOnce, Some(range))
            if range == "bytes=0-0" && attempts.fetch_add(1, Ordering::Relaxed) == 0 =>
        {
            (
                "429 Too Many Requests",
                Vec::new(),
                "Retry-After: 1\r\n".into(),
            )
        }
        (RangeMode::ProbeThrottleOnce, Some(range)) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::Probe408Once, Some(range))
            if range == "bytes=0-0" && attempts.fetch_add(1, Ordering::Relaxed) == 0 =>
        {
            ("408 Request Timeout", Vec::new(), String::new())
        }
        (RangeMode::ProbeServerErrorOnce, Some(range))
            if range == "bytes=0-0" && attempts.fetch_add(1, Ordering::Relaxed) == 0 =>
        {
            ("503 Service Unavailable", Vec::new(), String::new())
        }
        (
            RangeMode::Probe408Once
            | RangeMode::ProbeServerErrorOnce
            | RangeMode::ProbeNetworkFailureOnce,
            Some(range),
        ) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::UnknownRangeTotal, Some(_)) => (
            "206 Partial Content",
            body[..1].to_vec(),
            "Content-Range: bytes 0-0/*\r\n".into(),
        ),
        (RangeMode::WorkerReturns200, Some(range)) if range != "bytes=0-0" => {
            ("200 OK", body.to_vec(), String::new())
        }
        (RangeMode::WorkerReturns416, Some(range)) if range != "bytes=0-0" => {
            ("416 Range Not Satisfiable", Vec::new(), String::new())
        }
        (RangeMode::WorkerInvalidContentRange, Some(range)) if range != "bytes=0-0" => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes 0-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::NoEtagSupported, Some(range))
        | (RangeMode::NoEtagChangedLastModified, Some(range))
        | (RangeMode::ChangedEtagSupported, Some(range))
        | (RangeMode::MaliciousFilename, Some(range))
        | (RangeMode::SensitiveResponseHeaders, Some(range)) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (
            RangeMode::WorkerReturns200
            | RangeMode::WorkerReturns416
            | RangeMode::WorkerInvalidContentRange,
            Some(range),
        ) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::Supported, Some(range)) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::SlowSupported, Some(range)) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::InvalidProof, Some(_)) => (
            "206 Partial Content",
            body[..1].to_vec(),
            format!("Content-Range: bytes 1-1/{}\r\n", body.len()),
        ),
        (RangeMode::ThrottleRangeOnce, Some(range))
            if range != "bytes=0-0" && attempts.fetch_add(1, Ordering::Relaxed) == 0 =>
        {
            (
                "429 Too Many Requests",
                Vec::new(),
                "Retry-After: 1\r\n".into(),
            )
        }
        (RangeMode::ThrottleRangeOnce, Some(range)) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::Throttle503Once, Some(range))
            if range != "bytes=0-0" && attempts.fetch_add(1, Ordering::Relaxed) == 0 =>
        {
            (
                "503 Service Unavailable",
                Vec::new(),
                "Retry-After: 1\r\n".into(),
            )
        }
        (RangeMode::Throttle503Once, Some(range)) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::ForbiddenAfterProof, Some(range)) if range != "bytes=0-0" => {
            ("403 Forbidden", Vec::new(), String::new())
        }
        (RangeMode::ForbiddenAfterProof, Some(range)) => {
            let (start, end) = parse_range(&range, body.len() as u64);
            let end = end.min(body.len() as u64 - 1);
            (
                "206 Partial Content",
                body[start as usize..=end as usize].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
            )
        }
        (RangeMode::SimpleRetryOnce, None) if attempts.fetch_add(1, Ordering::Relaxed) == 0 => (
            "503 Service Unavailable",
            Vec::new(),
            "Retry-After: 0\r\n".into(),
        ),
        (RangeMode::AlwaysRetrySimple, None) => (
            "503 Service Unavailable",
            Vec::new(),
            "Retry-After: 0\r\n".into(),
        ),
        _ => ("200 OK", body.to_vec(), String::new()),
    };
    let identity_headers = match mode {
        RangeMode::NoEtagSupported | RangeMode::NoEtagChangedLastModified => String::new(),
        RangeMode::ChangedEtagSupported => "ETag: \"fixture-v2\"\r\n".into(),
        _ => "ETag: \"fixture-v1\"\r\n".into(),
    };
    let last_modified = if matches!(mode, RangeMode::NoEtagChangedLastModified) {
        "Wed, 02 Jan 2030 00:00:00 GMT"
    } else {
        "Tue, 01 Jan 2030 00:00:00 GMT"
    };
    let fixture_headers = match mode {
        RangeMode::MaliciousFilename => {
            "Content-Disposition: attachment; filename=../../SENTINEL_FILENAME\r\n"
        }
        RangeMode::SensitiveResponseHeaders => {
            "Set-Cookie: session=SENTINEL_HEADER_SECRET\r\nAuthorization: Bearer SENTINEL_AUTH_SECRET\r\n"
        }
        _ => "",
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{identity_headers}Last-Modified: {last_modified}\r\nConnection: close\r\n{fixture_headers}{extra}\r\n",
        response_body.len()
    );
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }
    if matches!(mode, RangeMode::SlowSupported) {
        for chunk in response_body.chunks(1024) {
            if stream.write_all(chunk).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(4));
        }
    } else {
        let _ = stream.write_all(&response_body);
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn parse_range(value: &str, max: u64) -> (u64, u64) {
    let value = value.trim().strip_prefix("bytes=").unwrap();
    let (start, end) = value.split_once('-').unwrap();
    (start.parse().unwrap(), end.parse().unwrap_or(max - 1))
}

fn temporary_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("downget-{name}-{nonce}"));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn run_add(
    url: &str,
    state: &std::path::Path,
    destination: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", url, "--output"])
        .arg(destination)
        .env("DOWNGET_STATE_DIR", state)
        .output()
        .unwrap()
}

fn run_add_with_checksum(
    url: &str,
    state: &std::path::Path,
    destination: &std::path::Path,
    checksum: &str,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", url, "--output"])
        .arg(destination)
        .args(["--sha256", checksum])
        .env("DOWNGET_STATE_DIR", state)
        .output()
        .unwrap()
}

fn run_add_default_output(
    url: &str,
    state: &std::path::Path,
    current_dir: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", url])
        .current_dir(current_dir)
        .env("DOWNGET_STATE_DIR", state)
        .output()
        .unwrap()
}

fn run_resume(id: i64, state: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["resume", &id.to_string()])
        .env("DOWNGET_STATE_DIR", state)
        .output()
        .unwrap()
}

fn run_resume_with_url(id: i64, state: &std::path::Path, url: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["resume", &id.to_string(), "--url", url])
        .env("DOWNGET_STATE_DIR", state)
        .output()
        .unwrap()
}

#[test]
fn direct_range_download_writes_the_final_file_after_a_valid_proof() {
    // Slightly over 16 MiB forces the static planner to create two segments
    // under its conservative default concurrency of two.
    let body = vec![0x5a; 16 * 1024 * 1024 + 97];
    let fixture = Fixture::start(RangeMode::Supported, body.clone());
    let root = temporary_root("range");
    let output = run_add(
        &fixture.url(),
        &root.join("state"),
        &root.join("archive.bin"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(root.join("archive.bin")).unwrap(), body);
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.first().unwrap(), "bytes=0-0");
    let segment_ranges: Vec<_> = requests
        .iter()
        .filter(|request| *request != "bytes=0-0")
        .collect();
    assert_eq!(segment_ranges.len(), 2);
    assert_ne!(segment_ranges[0], segment_ranges[1]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_200_range_probe_falls_back_to_a_single_simple_download() {
    let body = b"simple fixture".to_vec();
    let fixture = Fixture::start(RangeMode::Unsupported, body.clone());
    let root = temporary_root("simple");
    let output = run_add(
        &fixture.url(),
        &root.join("state"),
        &root.join("simple.bin"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(root.join("simple.bin")).unwrap(), body);
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.as_slice(), ["bytes=0-0", "no-range"]);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn invalid_range_proof_is_not_accepted_as_segment_data() {
    let fixture = Fixture::start(RangeMode::InvalidProof, b"invalid".to_vec());
    let client = source::client().unwrap();
    assert!(source::probe(&client, &fixture.url()).await.is_err());
}

#[test]
fn signed_url_is_not_written_to_sqlite_or_printed() {
    let fixture = Fixture::start(RangeMode::Supported, b"private fixture".to_vec());
    let root = temporary_root("redaction");
    let sensitive_url = format!("{}?token=SENTINEL_SECRET", fixture.url());
    let output = run_add(
        &sensitive_url,
        &root.join("state"),
        &root.join("private.bin"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = std::fs::read(root.join("state/state.sqlite3")).unwrap();
    let combined_output = [output.stdout, output.stderr].concat();
    assert!(!String::from_utf8_lossy(&state).contains("SENTINEL_SECRET"));
    assert!(!String::from_utf8_lossy(&combined_output).contains("SENTINEL_SECRET"));
    let listed = Command::new(env!("CARGO_BIN_EXE_downget"))
        .arg("list")
        .env("DOWNGET_STATE_DIR", root.join("state"))
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("SENTINEL_SECRET"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sensitive_url_and_response_headers_never_reach_output_or_sqlite() {
    let fixture = Fixture::start(
        RangeMode::SensitiveResponseHeaders,
        b"private response headers".to_vec(),
    );
    let root = temporary_root("redaction-headers");
    let state_root = root.join("state");
    let sensitive_url = format!("{}?token=SENTINEL_URL_SECRET", fixture.url());
    let output = run_add(&sensitive_url, &state_root, &root.join("private.bin"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state = std::fs::read(state_root.join("state.sqlite3")).unwrap();
    let terminal = [output.stdout, output.stderr].concat();
    let listed = Command::new(env!("CARGO_BIN_EXE_downget"))
        .arg("list")
        .env("DOWNGET_STATE_DIR", &state_root)
        .output()
        .unwrap();
    let all_visible = [terminal, listed.stdout, listed.stderr].concat();
    for secret in [
        "SENTINEL_URL_SECRET",
        "SENTINEL_HEADER_SECRET",
        "SENTINEL_AUTH_SECRET",
    ] {
        assert!(
            !String::from_utf8_lossy(&state).contains(secret),
            "SQLite: {secret}"
        );
        assert!(
            !String::from_utf8_lossy(&all_visible).contains(secret),
            "terminal: {secret}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_transient_simple_transfer_failure_is_retried() {
    let fixture = Fixture::start(RangeMode::SimpleRetryOnce, b"retry fixture".to_vec());
    let root = temporary_root("retry");
    let output = run_add(&fixture.url(), &root.join("state"), &root.join("retry.bin"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(root.join("retry.bin")).unwrap(),
        b"retry fixture"
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.as_slice(), ["bytes=0-0", "no-range", "no-range"]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn simple_resume_announces_discard_and_restarts_at_byte_zero() {
    let body = b"fresh simple payload".to_vec();
    let fixture = Fixture::start(RangeMode::Unsupported, body.clone());
    let root = temporary_root("simple-resume");
    let state = root.join("state");
    let store = Store::open(state.clone()).unwrap();
    let id = store.create_initial_job(2, None).unwrap();
    let destination = root.join("simple.bin");
    let partial = root.join("simple.bin.part");
    store
        .initialize_job(
            id,
            &destination,
            &partial,
            TransferMode::Simple,
            Some(body.len() as u64),
            Some("\"fixture-v1\""),
            Some("Tue, 01 Jan 2030 00:00:00 GMT"),
            UrlMode::Retained,
            Some(&fixture.url()),
            &fixture.url(),
        )
        .unwrap();
    PartFile::create_new(&partial, None)
        .unwrap()
        .append(b"stale partial")
        .unwrap();
    for _ in 0..4 {
        store.begin_simple_attempt(id).unwrap();
    }
    store
        .set_state(
            id,
            JobState::FailedTerminal,
            Some("network"),
            Some("resume"),
        )
        .unwrap();

    let output = run_resume(id, &state);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("descartando o parcial"));
    assert_eq!(std::fs::read(destination).unwrap(), body);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn checksum_and_global_concurrency_cli_contracts_are_persistent() {
    let fixture = Fixture::start(RangeMode::Supported, b"abc".to_vec());
    let root = temporary_root("checksum-config");
    let state = root.join("state");
    for value in ["1", "8"] {
        let output = Command::new(env!("CARGO_BIN_EXE_downget"))
            .args(["config", "set", "concurrency", value])
            .env("DOWNGET_STATE_DIR", &state)
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    let invalid = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["config", "set", "concurrency", "0"])
        .env("DOWNGET_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(
        Store::open(state.clone())
            .unwrap()
            .get_concurrency()
            .unwrap(),
        8
    );

    let correct = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let completed = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", &fixture.url(), "--output"])
        .arg(root.join("checksum-ok"))
        .args(["--sha256", correct])
        .env("DOWNGET_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        completed.status.success(),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(std::fs::read(root.join("checksum-ok")).unwrap(), b"abc");

    let invalid_format = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", &fixture.url(), "--sha256", "not-a-digest"])
        .env("DOWNGET_STATE_DIR", &state)
        .output()
        .unwrap();
    assert_eq!(invalid_format.status.code(), Some(2));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn checksum_mismatch_fails_finalization_and_preserves_partial() {
    let fixture = Fixture::start(RangeMode::Supported, b"checksum payload".to_vec());
    let root = temporary_root("checksum-mismatch");
    let state = root.join("state");
    let destination = root.join("checksum.bin");
    let wrong_digest = "00".repeat(32);
    let output = run_add_with_checksum(&fixture.url(), &state, &destination, &wrong_digest);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256 não confere"));
    assert!(!destination.exists());
    assert_eq!(
        std::fs::read(root.join("checksum.bin.part")).unwrap(),
        b"checksum payload"
    );
    let job = Store::open(state).unwrap().job(1).unwrap();
    assert_eq!(job.state, JobState::FailedTerminal);
    assert_eq!(job.sha256_expected.as_deref(), Some(wrong_digest.as_str()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn existing_output_is_never_overwritten() {
    let fixture = Fixture::start(RangeMode::Supported, b"new bytes".to_vec());
    let root = temporary_root("collision");
    let destination = root.join("existing.bin");
    std::fs::write(&destination, b"original bytes").unwrap();
    let output = run_add(&fixture.url(), &root.join("state"), &destination);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(destination).unwrap(), b"original bytes");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unsafe_content_disposition_uses_download_id_fallback_and_never_overwrites_it() {
    let fixture = Fixture::start(RangeMode::MaliciousFilename, b"safe name".to_vec());
    let root = temporary_root("fallback-name");
    let state = root.join("state");
    let output = run_add_default_output(&fixture.url(), &state, &root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(root.join("download-1")).unwrap(),
        b"safe name"
    );
    assert!(!root.join("SENTINEL_FILENAME").exists());

    let collision_root = temporary_root("fallback-collision");
    let collision_state = collision_root.join("state");
    let fallback = collision_root.join("download-1");
    std::fs::write(&fallback, b"do not overwrite").unwrap();
    let collision = run_add_default_output(&fixture.url(), &collision_state, &collision_root);
    assert!(!collision.status.success());
    assert_eq!(std::fs::read(fallback).unwrap(), b"do not overwrite");
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(collision_root).unwrap();
}

#[test]
fn parallel_429_reduces_concurrency_and_is_visible_in_list() {
    let fixture = Fixture::start(RangeMode::ThrottleRangeOnce, b"throttled".to_vec());
    let root = temporary_root("parallelism");
    let started = std::time::Instant::now();
    let output = run_add(
        &fixture.url(),
        &root.join("state"),
        &root.join("throttled.bin"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed = Command::new(env!("CARGO_BIN_EXE_downget"))
        .arg("list")
        .env("DOWNGET_STATE_DIR", root.join("state"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("reduced_after_429_or_503"));
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "Retry-After must delay requeue"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn parallel_503_reduces_concurrency_waits_and_persists_the_list_note() {
    let fixture = Fixture::start(
        RangeMode::Throttle503Once,
        vec![0x71; 16 * 1024 * 1024 + 11],
    );
    let root = temporary_root("parallelism-503");
    let state = root.join("state");
    let started = std::time::Instant::now();
    let output = run_add(&fixture.url(), &state, &root.join("throttled.bin"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "Retry-After must delay the 503 requeue"
    );
    let job = Store::open(state.clone()).unwrap().job(1).unwrap();
    assert_eq!(job.effective_concurrency, 1);
    assert_eq!(
        job.parallelism_note.as_deref(),
        Some("reduced_after_429_or_503")
    );
    let listed = Command::new(env!("CARGO_BIN_EXE_downget"))
        .arg("list")
        .env("DOWNGET_STATE_DIR", state)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("reduced_after_429_or_503"));
    let worker_ranges = fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.as_str() != "bytes=0-0")
        .count();
    assert!(worker_ranges >= 3, "two planned ranges plus the 503 retry");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn probe_retries_429_before_confirming_range() {
    let fixture = Fixture::start(RangeMode::ProbeThrottleOnce, b"probe retry".to_vec());
    let root = temporary_root("probe-retry");
    let output = run_add(&fixture.url(), &root.join("state"), &root.join("probe.bin"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let proof_count = fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.as_str() == "bytes=0-0")
        .count();
    assert_eq!(proof_count, 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn probe_retries_transport_408_and_5xx_before_confirming_range() {
    for (mode, name) in [
        (RangeMode::ProbeNetworkFailureOnce, "probe-network"),
        (RangeMode::Probe408Once, "probe-408"),
        (RangeMode::ProbeServerErrorOnce, "probe-503"),
    ] {
        let fixture = Fixture::start(mode, b"probe retry".to_vec());
        let root = temporary_root(name);
        let output = run_add(&fixture.url(), &root.join("state"), &root.join("probe.bin"));
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let proof_count = fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.as_str() == "bytes=0-0")
            .count();
        assert_eq!(proof_count, 2, "{name}");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn unknown_range_total_falls_back_to_simple_without_promoting_probe_data() {
    let fixture = Fixture::start(RangeMode::UnknownRangeTotal, b"unknown total".to_vec());
    let root = temporary_root("unknown-total");
    let output = run_add(
        &fixture.url(),
        &root.join("state"),
        &root.join("unknown.bin"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(root.join("unknown.bin")).unwrap(),
        b"unknown total"
    );
    assert_eq!(
        fixture.requests.lock().unwrap().as_slice(),
        ["bytes=0-0", "no-range"]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn worker_200_after_valid_proof_stops_without_final_file() {
    let fixture = Fixture::start(
        RangeMode::WorkerReturns200,
        vec![0x66; 16 * 1024 * 1024 + 5],
    );
    let root = temporary_root("worker-200");
    let destination = root.join("unsafe.bin");
    let output = run_add(&fixture.url(), &root.join("state"), &destination);
    assert!(!output.status.success());
    assert!(!destination.exists());
    assert!(root.join("unsafe.bin.part").exists());
    let ranges = fixture.requests.lock().unwrap();
    let worker_requests = ranges
        .iter()
        .filter(|request| request.as_str() != "bytes=0-0")
        .count();
    assert!(
        worker_requests <= 2,
        "no Range retry may start after protocol failure"
    );
    assert_eq!(
        Store::open(root.join("state"))
            .unwrap()
            .job(1)
            .unwrap()
            .state,
        JobState::RequiresReinspect
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn post_proof_416_and_invalid_content_range_require_reinspection() {
    for (mode, name) in [
        (RangeMode::WorkerReturns416, "worker-416"),
        (RangeMode::WorkerInvalidContentRange, "worker-invalid-range"),
    ] {
        let fixture = Fixture::start(mode, vec![0x6a; 16 * 1024 * 1024 + 5]);
        let root = temporary_root(name);
        let state = root.join("state");
        let destination = root.join("unsafe.bin");
        let output = run_add(&fixture.url(), &state, &destination);
        assert!(!output.status.success(), "{name}");
        assert!(!destination.exists(), "{name}");
        assert!(root.join("unsafe.bin.part").exists(), "{name}");
        assert_eq!(
            Store::open(state).unwrap().job(1).unwrap().state,
            JobState::RequiresReinspect,
            "{name}"
        );
        let worker_requests = fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.as_str() != "bytes=0-0")
            .count();
        assert!(
            worker_requests <= 2,
            "{name}: no worker may retry after reinspection is required"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn replacement_url_with_matching_identity_resumes_a_preserved_segmented_job() {
    let body = b"replacement source".to_vec();
    let fixture = Fixture::start(RangeMode::Supported, body.clone());
    let root = temporary_root("replacement");
    let state = root.join("state");
    let store = Store::open(state.clone()).unwrap();
    let id = store.create_initial_job(2, None).unwrap();
    let destination = root.join("replacement.bin");
    let partial = root.join("replacement.bin.part");
    store
        .initialize_job(
            id,
            &destination,
            &partial,
            TransferMode::Segmented,
            Some(body.len() as u64),
            Some("\"fixture-v1\""),
            Some("Tue, 01 Jan 2030 00:00:00 GMT"),
            UrlMode::ReplacementRequired,
            None,
            "http://fixture.invalid/file",
        )
        .unwrap();
    PartFile::create_new(&partial, Some(body.len() as u64)).unwrap();
    store
        .create_segments(id, &[(0, body.len() as u64 - 1)])
        .unwrap();
    store
        .set_state(
            id,
            JobState::Paused,
            Some("awaiting_url"),
            Some("replacement"),
        )
        .unwrap();

    let output = run_resume_with_url(id, &state, &fixture.url());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(destination).unwrap(), body);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resume_requests_only_the_durably_missing_segment() {
    let body = vec![0x33; 16 * 1024 * 1024 + 31];
    let fixture = Fixture::start(RangeMode::Supported, body.clone());
    let root = temporary_root("resume-segments");
    let state = root.join("state");
    let store = Store::open(state.clone()).unwrap();
    let id = store.create_initial_job(2, None).unwrap();
    let destination = root.join("resume.bin");
    let partial = root.join("resume.bin.part");
    let first_end = body.len() as u64 / 2;
    let second_start = first_end + 1;
    store
        .initialize_job(
            id,
            &destination,
            &partial,
            TransferMode::Segmented,
            Some(body.len() as u64),
            Some("\"fixture-v1\""),
            Some("Tue, 01 Jan 2030 00:00:00 GMT"),
            UrlMode::Retained,
            Some(&fixture.url()),
            &fixture.url(),
        )
        .unwrap();
    let part = PartFile::create_new(&partial, Some(body.len() as u64)).unwrap();
    part.write_at(0, &body[..=first_end as usize]).unwrap();
    part.sync().unwrap();
    store
        .create_segments(id, &[(0, first_end), (second_start, body.len() as u64 - 1)])
        .unwrap();
    let mut segments = store.segments(id).unwrap();
    let first = segments.remove(0);
    store.checkpoint_segment(id, &first, 1).unwrap();
    store
        .set_state(id, JobState::Paused, Some("interrupted"), Some("resume"))
        .unwrap();

    let output = run_resume(id, &state);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(destination).unwrap(), body);
    let requests = fixture.requests.lock().unwrap();
    assert!(requests
        .iter()
        .any(|request| request == &format!("bytes={second_start}-{}", body.len() - 1)));
    assert!(!requests
        .iter()
        .any(|request| request == &format!("bytes=0-{first_end}")));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn changed_source_identity_blocks_resume_before_touching_the_partial() {
    let fixture = Fixture::start(RangeMode::Supported, b"identity source".to_vec());
    let root = temporary_root("identity-mismatch");
    let state = root.join("state");
    let store = Store::open(state.clone()).unwrap();
    let id = store.create_initial_job(2, None).unwrap();
    let destination = root.join("identity.bin");
    let partial = root.join("identity.bin.part");
    store
        .initialize_job(
            id,
            &destination,
            &partial,
            TransferMode::Segmented,
            Some(15),
            Some("\"old-version\""),
            Some("Tue, 01 Jan 2030 00:00:00 GMT"),
            UrlMode::Retained,
            Some(&fixture.url()),
            &fixture.url(),
        )
        .unwrap();
    PartFile::create_new(&partial, Some(15))
        .unwrap()
        .write_at(0, b"preserve-this")
        .unwrap();
    let before = std::fs::read(&partial).unwrap();
    let output = run_resume(id, &state);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("identidade da fonte mudou"));
    assert_eq!(std::fs::read(&partial).unwrap(), before);
    assert!(!destination.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resume_without_etag_uses_size_and_last_modified_and_rejects_a_changed_date() {
    let body = b"identity without etag".to_vec();
    let fixture = Fixture::start(RangeMode::NoEtagSupported, body.clone());
    let root = temporary_root("identity-no-etag");
    let state = root.join("state");
    let store = Store::open(state.clone()).unwrap();
    let id = store.create_initial_job(2, None).unwrap();
    let destination = root.join("compatible.bin");
    let partial = root.join("compatible.bin.part");
    store
        .initialize_job(
            id,
            &destination,
            &partial,
            TransferMode::Segmented,
            Some(body.len() as u64),
            None,
            Some("Tue, 01 Jan 2030 00:00:00 GMT"),
            UrlMode::Retained,
            Some(&fixture.url()),
            &fixture.url(),
        )
        .unwrap();
    PartFile::create_new(&partial, Some(body.len() as u64)).unwrap();
    store
        .create_segments(id, &[(0, body.len() as u64 - 1)])
        .unwrap();
    store
        .set_state(id, JobState::Paused, Some("interrupted"), Some("resume"))
        .unwrap();
    let resumed = run_resume(id, &state);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(std::fs::read(destination).unwrap(), body);

    let changed_fixture = Fixture::start(
        RangeMode::NoEtagChangedLastModified,
        b"identity without etag".to_vec(),
    );
    let changed_root = temporary_root("identity-date-mismatch");
    let changed_state = changed_root.join("state");
    let changed_store = Store::open(changed_state.clone()).unwrap();
    let changed_id = changed_store.create_initial_job(2, None).unwrap();
    let changed_destination = changed_root.join("blocked.bin");
    let changed_partial = changed_root.join("blocked.bin.part");
    changed_store
        .initialize_job(
            changed_id,
            &changed_destination,
            &changed_partial,
            TransferMode::Segmented,
            Some(body.len() as u64),
            None,
            Some("Tue, 01 Jan 2030 00:00:00 GMT"),
            UrlMode::Retained,
            Some(&changed_fixture.url()),
            &changed_fixture.url(),
        )
        .unwrap();
    let changed_part = PartFile::create_new(&changed_partial, Some(body.len() as u64)).unwrap();
    changed_part.write_at(0, b"preserve").unwrap();
    changed_part.sync().unwrap();
    changed_store
        .create_segments(changed_id, &[(0, body.len() as u64 - 1)])
        .unwrap();
    changed_store
        .set_state(
            changed_id,
            JobState::Paused,
            Some("interrupted"),
            Some("resume"),
        )
        .unwrap();
    let before = std::fs::read(&changed_partial).unwrap();
    let rejected = run_resume(changed_id, &changed_state);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("identidade da fonte mudou"));
    assert_eq!(std::fs::read(changed_partial).unwrap(), before);
    assert!(!changed_destination.exists());
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(changed_root).unwrap();
}

#[test]
fn divergent_replacement_url_blocks_and_preserves_the_partial() {
    let body = b"replacement identity".to_vec();
    let replacement = Fixture::start(RangeMode::ChangedEtagSupported, body.clone());
    let root = temporary_root("replacement-divergent");
    let state = root.join("state");
    let store = Store::open(state.clone()).unwrap();
    let id = store.create_initial_job(2, None).unwrap();
    let destination = root.join("replacement.bin");
    let partial = root.join("replacement.bin.part");
    store
        .initialize_job(
            id,
            &destination,
            &partial,
            TransferMode::Segmented,
            Some(body.len() as u64),
            Some("\"fixture-v1\""),
            Some("Tue, 01 Jan 2030 00:00:00 GMT"),
            UrlMode::ReplacementRequired,
            None,
            "[fonte redigida]",
        )
        .unwrap();
    let part = PartFile::create_new(&partial, Some(body.len() as u64)).unwrap();
    part.write_at(0, b"preserve").unwrap();
    part.sync().unwrap();
    store
        .create_segments(id, &[(0, body.len() as u64 - 1)])
        .unwrap();
    store
        .set_state(
            id,
            JobState::Paused,
            Some("awaiting_url"),
            Some("replacement"),
        )
        .unwrap();
    let before = std::fs::read(&partial).unwrap();
    let output = run_resume_with_url(id, &state, &replacement.url());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("identidade da fonte mudou"));
    assert_eq!(std::fs::read(partial).unwrap(), before);
    assert!(!destination.exists());
    assert_eq!(
        Store::open(state).unwrap().job(id).unwrap().state,
        JobState::Paused
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn forbidden_signed_source_pauses_without_leaking_its_url() {
    let fixture = Fixture::start(RangeMode::ForbiddenAfterProof, b"403 fixture".to_vec());
    let root = temporary_root("forbidden");
    let state = root.join("state");
    let sensitive_url = format!("{}?signature=SENTINEL_403", fixture.url());
    let output = run_add(&sensitive_url, &state, &root.join("forbidden.bin"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--url <NOVA_URL>"));
    assert!(!stderr.contains("SENTINEL_403"));
    let requests_before_resume = fixture.requests.lock().unwrap().len();
    let without_replacement = run_resume(1, &state);
    assert!(!without_replacement.status.success());
    assert!(String::from_utf8_lossy(&without_replacement.stderr).contains("--url <NOVA_URL>"));
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        requests_before_resume
    );
    let listed = Command::new(env!("CARGO_BIN_EXE_downget"))
        .arg("list")
        .env("DOWNGET_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("--url <NOVA_URL>"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn second_process_cancel_acknowledges_pause_before_discarding() {
    let fixture = Fixture::start(RangeMode::SlowSupported, vec![0x44; 256 * 1024]);
    let root = temporary_root("cancel-process");
    let state = root.join("state");
    let destination = root.join("slow.bin");
    let mut add = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", &fixture.url(), "--output"])
        .arg(&destination)
        .env("DOWNGET_STATE_DIR", &state)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if Store::open(state.clone())
            .ok()
            .and_then(|store| store.job(1).ok())
            .is_some_and(|job| job.state == JobState::RunningSegmented)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "add did not create Job 1"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // Let the owner pass the tiny state→lease handoff before issuing the
    // second-process command. This exercises the ack path, not early cancel.
    thread::sleep(Duration::from_millis(100));
    let cancelled = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["cancel", "1", "--discard"])
        .env("DOWNGET_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    assert!(add.wait().unwrap().success());
    assert!(!destination.exists());
    assert!(!root.join("slow.bin.part").exists());
    assert!(Store::open(state.clone()).unwrap().job(1).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sigint_pauses_durably_and_returns_exit_130() {
    let fixture = Fixture::start(RangeMode::SlowSupported, vec![0x55; 256 * 1024]);
    let root = temporary_root("sigint");
    let state = root.join("state");
    let destination = root.join("sigint.bin");
    let mut add = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", &fixture.url(), "--output"])
        .arg(&destination)
        .env("DOWNGET_STATE_DIR", &state)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if Store::open(state.clone())
            .ok()
            .and_then(|store| store.job(1).ok())
            .is_some_and(|job| job.state == JobState::RunningSegmented)
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "add did not start");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(100));
    let pid = add.id().to_string();
    assert!(Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .unwrap()
        .success());
    let status = add.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    assert!(!destination.exists());
    assert_eq!(
        Store::open(state.clone()).unwrap().job(1).unwrap().state,
        JobState::Paused
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sigint_then_resume_uses_only_the_durably_missing_range() {
    let body = vec![0x57; 256 * 1024];
    let fixture = Fixture::start(RangeMode::SlowSupported, body.clone());
    let root = temporary_root("sigint-resume");
    let state = root.join("state");
    let destination = root.join("resumed.bin");
    let mut add = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", &fixture.url(), "--output"])
        .arg(&destination)
        .env("DOWNGET_STATE_DIR", &state)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if Store::open(state.clone())
            .ok()
            .and_then(|store| store.job(1).ok())
            .is_some_and(|job| job.state == JobState::RunningSegmented)
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "add did not start");
        thread::sleep(Duration::from_millis(20));
    }
    // Force the exact critical window: the worker has written data to the
    // preallocated `.part`, but its periodic checkpoint has not yet run.
    // SIGINT must cause that prefix to be synced and checkpointed before the
    // process reports the paused outcome.
    let partial = root.join("resumed.bin.part");
    loop {
        let committed_end =
            Store::open(state.clone()).unwrap().segments(1).unwrap()[0].committed_end;
        let first_byte_was_written = std::fs::read(&partial)
            .ok()
            .is_some_and(|bytes| bytes.first() == Some(&0x57));
        if first_byte_was_written && committed_end == -1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker did not enter the uncheckpointed-write window"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let pid = add.id().to_string();
    assert!(Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .unwrap()
        .success());
    assert_eq!(add.wait().unwrap().code(), Some(130));
    let committed_end = Store::open(state.clone()).unwrap().segments(1).unwrap()[0].committed_end;
    assert!(committed_end >= 0 && committed_end < body.len() as i64 - 1);
    assert_eq!(
        Store::open(state.clone()).unwrap().job(1).unwrap().state,
        JobState::Paused
    );

    let resumed = run_resume(1, &state);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(std::fs::read(destination).unwrap(), body);
    let expected = format!("bytes={}-{}", committed_end + 1, body.len() - 1);
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|range| range == &expected),
        "resume did not start at the durable checkpoint"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn interrupted_signed_url_is_not_persisted_and_resume_requires_a_replacement_without_network() {
    let fixture = Fixture::start(RangeMode::SlowSupported, vec![0x58; 256 * 1024]);
    let root = temporary_root("signed-interrupted");
    let state = root.join("state");
    let destination = root.join("private.bin");
    let signed_url = format!("{}?signature=SENTINEL_INTERRUPTED_SECRET", fixture.url());
    let add = Command::new(env!("CARGO_BIN_EXE_downget"))
        .args(["add", &signed_url, "--output"])
        .arg(&destination)
        .env("DOWNGET_STATE_DIR", &state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if Store::open(state.clone())
            .ok()
            .and_then(|store| store.job(1).ok())
            .is_some_and(|job| job.state == JobState::RunningSegmented)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "signed add did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(100));
    let pid = add.id().to_string();
    assert!(Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .unwrap()
        .success());
    let interrupted = add.wait_with_output().unwrap();
    assert_eq!(interrupted.status.code(), Some(130));
    let interrupted_terminal = [interrupted.stdout, interrupted.stderr].concat();
    assert!(!String::from_utf8_lossy(&interrupted_terminal).contains("SENTINEL_INTERRUPTED_SECRET"));
    let state_bytes = std::fs::read(state.join("state.sqlite3")).unwrap();
    assert!(!String::from_utf8_lossy(&state_bytes).contains("SENTINEL_INTERRUPTED_SECRET"));
    assert_eq!(
        Store::open(state.clone()).unwrap().job(1).unwrap().url_mode,
        UrlMode::ReplacementRequired
    );
    let before_resume_requests = fixture.requests.lock().unwrap().len();
    let resume = run_resume(1, &state);
    assert!(!resume.status.success());
    let terminal = [resume.stdout, resume.stderr].concat();
    assert!(String::from_utf8_lossy(&terminal).contains("--url <NOVA_URL>"));
    assert!(!String::from_utf8_lossy(&terminal).contains("SENTINEL_INTERRUPTED_SECRET"));
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        before_resume_requests
    );
    assert!(!destination.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn list_covers_all_job_states_with_safe_next_actions_and_no_source_leak() {
    let root = temporary_root("list-states");
    let state_root = root.join("state");
    let store = Store::open(state_root.clone()).unwrap();
    let cases = [
        JobState::Initializing,
        JobState::Probing,
        JobState::RunningSimple,
        JobState::RunningSegmented,
        JobState::Paused,
        JobState::RequiresReinspect,
        JobState::FailedTerminal,
        JobState::Finalizing,
        JobState::Completed,
    ];
    for (index, state) in cases.iter().enumerate() {
        let id = store.create_initial_job(2, None).unwrap();
        let destination = root.join(format!("job-{id}.bin"));
        let partial = root.join(format!("job-{id}.bin.part"));
        let replacement = *state == JobState::Paused;
        store
            .initialize_job(
                id,
                &destination,
                &partial,
                if *state == JobState::RunningSegmented {
                    TransferMode::Segmented
                } else {
                    TransferMode::Simple
                },
                Some(3),
                Some("\"fixture-v1\""),
                Some("Tue, 01 Jan 2030 00:00:00 GMT"),
                if replacement {
                    UrlMode::ReplacementRequired
                } else {
                    UrlMode::Retained
                },
                (!replacement).then_some("http://fixture.invalid/file?token=LIST_SECRET"),
                "http://fixture.invalid/file?token=LIST_SECRET",
            )
            .unwrap();
        store
            .set_state(id, *state, Some("fixture"), Some("fixture"))
            .unwrap();
        assert_eq!(id as usize, index + 1);
    }
    let output = Command::new(env!("CARGO_BIN_EXE_downget"))
        .arg("list")
        .env("DOWNGET_STATE_DIR", &state_root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let listed = String::from_utf8_lossy(&output.stdout);
    for state in cases {
        assert!(
            listed.contains(state.as_str()),
            "missing {}",
            state.as_str()
        );
    }
    assert!(listed.contains("em andamento"));
    assert!(listed.contains("use `downget resume 5 --url <NOVA_URL>`"));
    assert!(listed.contains("use `downget resume 6`"));
    assert!(listed.contains("use `downget resume 7`"));
    assert!(listed.contains("concluído"));
    assert!(!listed.contains("LIST_SECRET"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn retry_exhaustion_persists_and_resume_never_sends_a_sixth_simple_request() {
    let fixture = Fixture::start(RangeMode::AlwaysRetrySimple, b"never delivered".to_vec());
    let root = temporary_root("retry-exhaustion");
    let state = root.join("state");
    let output = run_add(&fixture.url(), &state, &root.join("retry.bin"));
    assert!(!output.status.success());
    let before_resume = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        before_resume
            .iter()
            .filter(|request| request.as_str() == "no-range")
            .count(),
        5
    );
    drop(before_resume);
    let resumed = run_resume(1, &state);
    assert!(!resumed.status.success());
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.as_str() == "no-range")
            .count(),
        5
    );
    assert_eq!(
        Store::open(state.clone())
            .unwrap()
            .begin_simple_attempt(1)
            .unwrap(),
        None
    );
    std::fs::remove_dir_all(root).unwrap();
}
