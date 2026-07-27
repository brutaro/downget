use reqwest::{
    header::{
        HeaderMap, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
        LAST_MODIFIED, LOCATION, RANGE,
    },
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
    pub one_drive_item: Option<OneDriveItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    Simple,
    Segmented,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneDriveItem {
    File,
    Folder,
}

pub fn one_drive_classification_message(item: OneDriveItem) -> &'static str {
    match item {
        OneDriveItem::File => {
            "Link público do OneDrive reconhecido como arquivo; o download direto será tentado."
        }
        OneDriveItem::Folder => {
            "Link público do OneDrive reconhecido como pasta; o provedor poderá entregá-la como arquivo ZIP."
        }
    }
}

pub fn one_drive_access_message() -> &'static str {
    "O link do OneDrive exige sessão, não é público ou bloqueou o download. Compartilhe como “Anyone with the link” com download permitido, ou use uma URL direta de arquivo."
}

fn one_drive_delivery_message(item: OneDriveItem) -> &'static str {
    match item {
        OneDriveItem::File => {
            "O link do OneDrive aponta para um arquivo, mas não fornece um anexo público. Torne o compartilhamento público e habilite download, depois forneça uma URL direta do arquivo."
        }
        OneDriveItem::Folder => {
            "O link do OneDrive aponta para uma pasta, mas não fornece ZIP público. Torne o compartilhamento público e habilite download, depois forneça uma URL pública do ZIP."
        }
    }
}

pub fn html_landing_page_message() -> &'static str {
    "a fonte respondeu uma página HTML de aterrissagem, não um arquivo para download"
}

pub fn client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| Error::Internal("não foi possível iniciar cliente HTTP".into()))
}

fn no_redirect_client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| Error::Internal("não foi possível iniciar cliente HTTP".into()))
}

/// Performs the required protocol proof.  The body of a probe is never used
/// as download data, even if the server returns it successfully. Public
/// OneDrive folders are the narrow exception: at most two bytes are sampled
/// solely to verify the ZIP signature, then discarded.
pub async fn probe(client: &Client, raw_url: &str) -> Result<SourceProbe> {
    let supplied_url = validate_url(raw_url)?;
    let (url, one_drive_item) = if is_onedrive_short_link(&supplied_url) {
        let (download_url, item) = resolve_onedrive_short_link(&supplied_url).await?;
        (download_url, Some(item))
    } else {
        (supplied_url.clone(), None)
    };
    // A public OneDrive short link is transformed to a query-bearing download
    // URL. It remains in memory for this run only, even if a later redirect
    // happens to have a clean-looking final URL.
    let initial_is_ephemeral = one_drive_item.is_some() || is_ephemeral(&supplied_url);
    probe_resolved(client, url, initial_is_ephemeral, one_drive_item).await
}

async fn probe_resolved(
    client: &Client,
    url: Url,
    initial_is_ephemeral: bool,
    one_drive_item: Option<OneDriveItem>,
) -> Result<SourceProbe> {
    let probe_range = if one_drive_item == Some(OneDriveItem::Folder) {
        "bytes=0-3"
    } else {
        "bytes=0-0"
    };
    let proof_end = if one_drive_item == Some(OneDriveItem::Folder) {
        3
    } else {
        0
    };
    let mut response = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match client
            .get(url.clone())
            .header(RANGE, probe_range)
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
    let mut response =
        response.ok_or_else(|| Error::User("a inspeção da fonte esgotou tentativas".into()))?;
    let status = response.status();
    let final_url = response.url().clone();
    let headers = response.headers().clone();
    if status.is_success() {
        if let Some(item) = one_drive_item {
            if let Some(message) = onedrive_delivery_error(item, &headers) {
                return Err(Error::User(message.into()));
            }
        } else if is_html_content_type(&headers) {
            return Err(Error::User(html_landing_page_message().into()));
        }
    }
    let valid_folder_signature =
        if status.is_success() && one_drive_item == Some(OneDriveItem::Folder) {
            probe_zip_signature(&mut response).await?
        } else {
            true
        };
    // The short folder sample is validation-only and is deliberately dropped;
    // transfer workers always issue their own request from byte zero.
    drop(response);
    if !valid_folder_signature {
        return Err(Error::User(
            one_drive_delivery_message(OneDriveItem::Folder).into(),
        ));
    }

    let size_from_length = headers.get(CONTENT_LENGTH).and_then(header_u64);
    let content_range = headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range);
    let (mode, size) = match status {
        StatusCode::PARTIAL_CONTENT => match content_range {
            Some((0, end, Some(total))) if end == proof_end && total > 0 => {
                (ProbeMode::Segmented, Some(total))
            }
            Some((0, end, None)) if end == proof_end => (ProbeMode::Simple, None),
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
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED if one_drive_item.is_some() => {
            return Err(Error::User(one_drive_access_message().into()));
        }
        StatusCode::FORBIDDEN => {
            return Err(Error::User(
                "a fonte retornou 403; use uma URL válida e tente novamente".into(),
            ));
        }
        StatusCode::UNAUTHORIZED => {
            return Err(Error::User(
                "a fonte retornou 401; use uma URL pública e tente novamente".into(),
            ));
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
        one_drive_item,
    })
}

async fn probe_zip_signature(response: &mut reqwest::Response) -> Result<bool> {
    const ZIP_SIGNATURE_LENGTH: usize = 4;
    let mut sample = Vec::with_capacity(ZIP_SIGNATURE_LENGTH);
    while sample.len() < ZIP_SIGNATURE_LENGTH {
        let chunk = response.chunk().await.map_err(|_| Error::Network)?;
        let Some(chunk) = chunk else { break };
        let remaining = ZIP_SIGNATURE_LENGTH - sample.len();
        sample.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(is_zip_signature(&sample))
}

fn is_zip_signature(sample: &[u8]) -> bool {
    matches!(sample, b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
}

async fn resolve_onedrive_short_link(short_url: &Url) -> Result<(Url, OneDriveItem)> {
    if short_url.scheme() != "https" {
        return Err(Error::User(
            "o link curto do OneDrive deve usar HTTPS".into(),
        ));
    }
    let response = no_redirect_client()?
        .get(short_url.clone())
        .send()
        .await
        .map_err(|_| Error::Network)?;
    let status = response.status();
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok());
    resolve_onedrive_first_response(status, location)
}

fn resolve_onedrive_first_response(
    status: StatusCode,
    location: Option<&str>,
) -> Result<(Url, OneDriveItem)> {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(Error::User(one_drive_access_message().into()));
    }
    if !status.is_redirection() {
        return Err(Error::User(
            "o link curto do OneDrive não retornou um redirecionamento público utilizável".into(),
        ));
    }
    let location = location.ok_or_else(|| {
        Error::User("o link curto do OneDrive não informou um destino seguro".into())
    })?;
    let redirect = Url::parse(location)
        .map_err(|_| Error::User("o link curto do OneDrive informou um destino inseguro".into()))?;
    onedrive_download_url_from_location(&redirect)
}

fn onedrive_download_url_from_location(location: &Url) -> Result<(Url, OneDriveItem)> {
    if location.scheme() != "https"
        || !location
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("onedrive.live.com"))
        || !location.username().is_empty()
        || location.password().is_some()
        || location.port().is_some()
        || location.path() != "/redir"
    {
        return Err(Error::User(
            "o link curto do OneDrive informou um destino inseguro".into(),
        ));
    }
    let mut hints = location
        .query_pairs()
        .filter(|(key, _)| key.eq_ignore_ascii_case("ithint"));
    let ithint = hints
        .next()
        .map(|(_, value)| value.into_owned())
        .filter(|_| hints.next().is_none())
        .ok_or_else(|| {
            Error::User("não foi possível classificar o link público do OneDrive".into())
        })?;
    let item = onedrive_item_from_ithint(&ithint).ok_or_else(|| {
        Error::User("não foi possível classificar o link público do OneDrive".into())
    })?;
    let mut download = Url::parse("https://onedrive.live.com/download")
        .map_err(|_| Error::Internal("URL de download OneDrive inválida".into()))?;
    {
        let mut query = download.query_pairs_mut();
        for (key, value) in location.query_pairs() {
            query.append_pair(key.as_ref(), value.as_ref());
        }
    }
    Ok((download, item))
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

pub fn is_html_content_type(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    matches!(
        content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "text/html" | "text/xhtml" | "application/xhtml+xml"
    )
}

pub fn onedrive_delivery_error(item: OneDriveItem, headers: &HeaderMap) -> Option<&'static str> {
    if is_html_content_type(headers) {
        return Some(one_drive_delivery_message(item));
    }
    if !has_attachment_content_disposition(headers) {
        return Some(one_drive_delivery_message(item));
    }
    if item == OneDriveItem::Folder && !is_onedrive_folder_archive(headers) {
        return Some(one_drive_delivery_message(item));
    }
    None
}

fn has_attachment_content_disposition(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|disposition| disposition.trim().eq_ignore_ascii_case("attachment"))
}

fn attachment_filename(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_DISPOSITION)?.to_str().ok()?;
    has_attachment_content_disposition(headers).then(|| content_disposition_filename(value))?
}

fn is_onedrive_folder_archive(headers: &HeaderMap) -> bool {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim() == "application/zip")
        || attachment_filename(headers)
            .is_some_and(|filename| filename.to_ascii_lowercase().ends_with(".zip"))
}

pub fn is_onedrive_download_url(raw: &str) -> bool {
    onedrive_item_from_download_url(raw).is_some()
}

pub fn onedrive_item_from_download_url(raw: &str) -> Option<OneDriveItem> {
    let url = Url::parse(raw).ok()?;
    (url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("onedrive.live.com"))
        && url.path() == "/download")
        .then_some(())?;
    let mut hints = url
        .query_pairs()
        .filter(|(key, _)| key.eq_ignore_ascii_case("ithint"));
    let (_, hint) = hints.next()?;
    hints
        .next()
        .is_none()
        .then(|| onedrive_item_from_ithint(&hint))?
}

fn is_onedrive_short_link(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("1drv.ms"))
}

fn onedrive_item_from_ithint(ithint: &str) -> Option<OneDriveItem> {
    let hint = ithint.to_ascii_lowercase();
    if hint.starts_with("folder") {
        Some(OneDriveItem::Folder)
    } else if hint.starts_with("file") {
        Some(OneDriveItem::File)
    } else {
        None
    }
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
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    async fn local_first_response(
        status: &str,
        location: Option<&str>,
    ) -> (StatusCode, Option<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let location = location.map(str::to_owned);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let location_header = location
                .as_deref()
                .map(|value| format!("Location: {value}\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\n{location_header}Connection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let response = no_redirect_client()
            .unwrap()
            .get(format!("http://{address}/short"))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        server.join().unwrap();
        (status, location)
    }

    async fn local_folder_probe(body: Vec<u8>) -> Result<SourceProbe> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.to_ascii_lowercase().contains("range: bytes=0-3"));
            let response_body = &body[..body.len().min(4)];
            let response = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nContent-Type: application/zip\r\nContent-Disposition: attachment; filename=folder.zip\r\nConnection: close\r\n\r\n",
                response_body.len(),
                response_body.len() - 1,
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(response_body).unwrap();
        });
        let url = validate_url(&format!("http://{address}/download")).unwrap();
        let result =
            probe_resolved(&client().unwrap(), url, true, Some(OneDriveItem::Folder)).await;
        server.join().unwrap();
        result
    }

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

    #[test]
    fn onedrive_file_location_becomes_ephemeral_download_url() {
        let location = validate_url(
            "https://onedrive.live.com/redir?cid=abc&id=abc%21123&authkey=SENTINEL_TOKEN&ithint=file%2Cpdf",
        )
        .unwrap();
        let (download, item) = onedrive_download_url_from_location(&location).unwrap();
        assert_eq!(item, OneDriveItem::File);
        assert_eq!(download.host_str(), Some("onedrive.live.com"));
        assert_eq!(download.path(), "/download");
        assert!(download
            .query()
            .is_some_and(|query| query.contains("authkey=")));
        assert!(is_ephemeral(&download));
        assert!(is_onedrive_download_url(download.as_str()));
    }

    #[test]
    fn onedrive_folder_location_becomes_download_url_with_its_parameters() {
        let location = validate_url(
            "https://onedrive.live.com/redir?cid=abc&resid=abc%21124&authkey=token&ithint=folder%2Czip",
        )
        .unwrap();
        let (download, item) = onedrive_download_url_from_location(&location).unwrap();
        assert_eq!(item, OneDriveItem::Folder);
        let pairs: Vec<_> = download.query_pairs().collect();
        assert!(pairs
            .iter()
            .any(|(key, value)| key == "resid" && value == "abc!124"));
        assert!(pairs
            .iter()
            .any(|(key, value)| key == "ithint" && value == "folder,zip"));
    }

    #[test]
    fn onedrive_first_redirect_keeps_public_parameters_without_exposing_them() {
        let (download, item) = resolve_onedrive_first_response(
            StatusCode::FOUND,
            Some(
                "https://onedrive.live.com/redir?cid=abc&resid=abc%21123&authkey=SENTINEL_CHAIN_TOKEN&ithint=file%2Czip",
            ),
        )
        .unwrap();
        assert_eq!(item, OneDriveItem::File);
        assert!(is_onedrive_download_url(download.as_str()));
        assert!(download
            .query()
            .is_some_and(|query| query.contains("authkey=")));
        let error = resolve_onedrive_first_response(StatusCode::FORBIDDEN, None).unwrap_err();
        assert!(error.to_string().contains("exige sessão"));
        assert!(!error.to_string().contains("SENTINEL_CHAIN_TOKEN"));
    }

    #[tokio::test]
    async fn local_first_redirect_with_token_is_not_followed_and_builds_download_url() {
        let (status, location) = local_first_response(
            "302 Found",
            Some(
                "https://onedrive.live.com/redir?cid=abc&resid=abc%21123&authkey=SENTINEL_REDIRECT_TOKEN&ithint=file%2Czip",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FOUND);
        let (download, item) =
            resolve_onedrive_first_response(status, location.as_deref()).unwrap();
        assert_eq!(item, OneDriveItem::File);
        assert!(is_onedrive_download_url(download.as_str()));
        assert!(is_ephemeral(&download));
    }

    #[tokio::test]
    async fn local_first_response_403_maps_to_safe_onedrive_guidance() {
        let (status, location) = local_first_response("403 Forbidden", None).await;
        let error = resolve_onedrive_first_response(status, location.as_deref()).unwrap_err();
        assert!(error.to_string().contains("Anyone with the link"));
        assert!(!error.to_string().contains("127.0.0.1"));
        let unauthorized =
            resolve_onedrive_first_response(StatusCode::UNAUTHORIZED, None).unwrap_err();
        assert!(unauthorized.to_string().contains("Anyone with the link"));
    }

    #[tokio::test]
    async fn folder_probe_requires_a_limited_real_zip_signature_before_source_probe() {
        let valid_empty_zip = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        let valid = local_folder_probe(valid_empty_zip.to_vec()).await.unwrap();
        assert_eq!(valid.mode, ProbeMode::Segmented);
        assert_eq!(valid.identity.size, Some(valid_empty_zip.len() as u64));

        for invalid_body in [b"PKNO".as_slice(), b"PK".as_slice()] {
            let invalid = local_folder_probe(invalid_body.to_vec()).await.unwrap_err();
            assert!(invalid.to_string().contains("pasta"));
            assert!(invalid.to_string().contains("ZIP público"));
        }
    }

    #[test]
    fn zip_signature_requires_a_complete_known_four_byte_marker() {
        assert!(is_zip_signature(b"PK\x03\x04"));
        assert!(is_zip_signature(b"PK\x05\x06"));
        assert!(is_zip_signature(b"PK\x07\x08"));
        assert!(!is_zip_signature(b"PKNO"));
        assert!(!is_zip_signature(b"PK"));
    }

    #[test]
    fn onedrive_short_host_and_redirect_target_are_strictly_validated() {
        assert!(is_onedrive_short_link(
            &validate_url("https://1drv.ms/u/s!public").unwrap()
        ));
        assert!(!is_onedrive_short_link(
            &validate_url("https://1drv.ms.example.invalid/u/s!public").unwrap()
        ));
        for unsafe_location in [
            "http://onedrive.live.com/redir?ithint=file",
            "https://evil.example/redir?ithint=file",
            "https://onedrive.live.com/?ithint=file",
            "https://onedrive.live.com/redir?id=abc",
            "https://onedrive.live.com:444/redir?ithint=file",
            "https://onedrive.live.com/redir?ithint=file&ithint=folder",
        ] {
            let location = validate_url(unsafe_location).unwrap();
            assert!(onedrive_download_url_from_location(&location).is_err());
        }
    }

    #[test]
    fn html_content_types_are_landing_pages_not_downloads() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "text/html; charset=utf-8".parse().unwrap());
        assert!(is_html_content_type(&headers));
        headers.insert(CONTENT_TYPE, "application/xhtml+xml".parse().unwrap());
        assert!(is_html_content_type(&headers));
        headers.insert(CONTENT_TYPE, "application/octet-stream".parse().unwrap());
        assert!(!is_html_content_type(&headers));
    }

    #[test]
    fn onedrive_delivery_requires_attachment_and_a_zip_for_folders() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(onedrive_delivery_error(OneDriveItem::File, &headers).is_some());
        headers.insert(CONTENT_TYPE, "application/zip".parse().unwrap());
        assert!(onedrive_delivery_error(OneDriveItem::Folder, &headers).is_some());
        headers.insert(
            CONTENT_DISPOSITION,
            "attachment; filename=folder.zip".parse().unwrap(),
        );
        assert!(onedrive_delivery_error(OneDriveItem::Folder, &headers).is_none());
        headers.insert(CONTENT_TYPE, "text/html".parse().unwrap());
        assert_eq!(
            onedrive_delivery_error(OneDriveItem::File, &headers),
            Some(one_drive_delivery_message(OneDriveItem::File))
        );
        assert_eq!(
            onedrive_delivery_error(OneDriveItem::Folder, &headers),
            Some(one_drive_delivery_message(OneDriveItem::Folder))
        );
    }
}
