use reqwest::{
    header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE},
    Client, StatusCode,
};
use url::Url;

use crate::{
    model::SourceIdentity,
    retry::{self, MAX_ATTEMPTS},
    ui, Error, Result,
};

#[derive(Debug, Clone)]
pub struct SourceProbe {
    pub url: String,
    pub source_display: String,
    pub ephemeral: bool,
    pub identity: SourceIdentity,
    pub mode: ProbeMode,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    Simple,
    Segmented,
}

pub fn client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| Error::Internal("não foi possível iniciar cliente HTTP".into()))
}

/// Performs the required protocol proof.  The body of a probe is never used
/// as download data, even if the server returns it successfully.
pub async fn probe(client: &Client, raw_url: &str) -> Result<SourceProbe> {
    let url = validate_url(raw_url)?;
    let initial_is_ephemeral = is_ephemeral(&url);
    let mut response = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match client
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .send()
            .await
        {
            Ok(candidate)
                if candidate.status() == StatusCode::REQUEST_TIMEOUT
                    || candidate.status() == StatusCode::TOO_MANY_REQUESTS
                    || candidate.status().is_server_error() =>
            {
                let status = candidate.status();
                let retry_after = retry::retry_after_header(candidate.headers());
                drop(candidate);
                if attempt == MAX_ATTEMPTS {
                    return Err(Error::User(format!(
                        "a inspeção da fonte esgotou tentativas após HTTP {}",
                        status.as_u16()
                    )));
                }
                tokio::time::sleep(retry::delay(attempt, retry_after.as_deref())).await;
            }
            Ok(candidate) => {
                response = Some(candidate);
                break;
            }
            Err(_) if attempt < MAX_ATTEMPTS => {
                tokio::time::sleep(retry::delay(attempt, None)).await
            }
            Err(_) => return Err(Error::Network),
        }
    }
    let response =
        response.ok_or_else(|| Error::User("a inspeção da fonte esgotou tentativas".into()))?;
    let status = response.status();
    let final_url = response.url().clone();
    let headers = response.headers().clone();
    // Explicitly consume/drop no bytes. reqwest will close the body on drop;
    // no probe response is ever passed to a transfer worker.
    drop(response);

    let size_from_length = headers.get(CONTENT_LENGTH).and_then(header_u64);
    let content_range = headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range);
    let (mode, size) = match status {
        StatusCode::PARTIAL_CONTENT => match content_range {
            Some((0, 0, Some(total))) if total > 0 => (ProbeMode::Segmented, Some(total)),
            Some((0, 0, None)) => (ProbeMode::Simple, None),
            _ => {
                return Err(Error::User(
                    "resposta Range insegura; o Job foi preservado para nova inspeção".into(),
                ))
            }
        },
        StatusCode::OK => (ProbeMode::Simple, size_from_length),
        StatusCode::RANGE_NOT_SATISFIABLE => {
            return Err(Error::User(
                "a fonte recusou a confirmação de intervalo (416)".into(),
            ))
        }
        _ if status.is_success() => (ProbeMode::Simple, size_from_length),
        StatusCode::FORBIDDEN => {
            return Err(Error::User(
                "a fonte retornou 403; use uma URL válida e tente novamente".into(),
            ))
        }
        _ => {
            return Err(Error::User(format!(
                "a fonte respondeu HTTP {}; tente novamente mais tarde",
                status.as_u16()
            )))
        }
    };
    let etag = header_string(&headers, ETAG);
    let last_modified = header_string(&headers, LAST_MODIFIED);
    let filename = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(content_disposition_filename);
    let final_raw = final_url.to_string();
    Ok(SourceProbe {
        url: final_raw.clone(),
        source_display: ui::source_display(&final_raw),
        // A redirect must never downgrade an initial signed URL to a retained
        // URL: the first location itself may have contained its credential.
        ephemeral: initial_is_ephemeral || is_ephemeral(&final_url),
        identity: SourceIdentity {
            size,
            etag,
            last_modified,
        },
        mode,
        filename,
    })
}

pub fn validate_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| Error::User("URL HTTP(S) inválida".into()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::User("a fonte deve usar HTTP ou HTTPS".into()));
    }
    if url.host_str().is_none() {
        return Err(Error::User("URL HTTP(S) sem host".into()));
    }
    Ok(url)
}

pub fn is_ephemeral(url: &Url) -> bool {
    url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
}

pub fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let value = value.trim();
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let total = if total == "*" {
        None
    } else {
        total.parse().ok()
    };
    Some((start.parse().ok()?, end.parse().ok()?, total))
}

pub fn content_disposition_filename(value: &str) -> Option<String> {
    value.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        if !key.eq_ignore_ascii_case("filename") {
            return None;
        }
        sanitize_filename(value.trim().trim_matches('"'))
    })
}

pub fn sanitize_filename(value: &str) -> Option<String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains("..")
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value.to_owned())
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn header_u64(value: &reqwest::header::HeaderValue) -> Option<u64> {
    value.to_str().ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_proof_requires_an_exact_parseable_range() {
        assert_eq!(parse_content_range("bytes 0-0/42"), Some((0, 0, Some(42))));
        assert_eq!(parse_content_range("bytes 0-2/42"), Some((0, 2, Some(42))));
        assert_eq!(parse_content_range("not-a-range"), None);
    }

    #[test]
    fn malicious_content_disposition_is_rejected() {
        assert_eq!(
            content_disposition_filename("attachment; filename=../../secret"),
            None
        );
        assert_eq!(
            content_disposition_filename("attachment; filename=archive.iso"),
            Some("archive.iso".into())
        );
    }

    #[test]
    fn signed_initial_url_stays_ephemeral_after_a_clean_redirect() {
        let initial = validate_url("https://example.test/start?signature=secret").unwrap();
        let final_url = validate_url("https://cdn.example.test/file.iso").unwrap();
        // This is the same classification applied after reqwest follows a
        // redirect, exercised without external network state.
        assert!(is_ephemeral(&initial) || is_ephemeral(&final_url));
    }
}
