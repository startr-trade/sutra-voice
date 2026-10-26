//! Where recordings are written: an object store with multipart uploads, so a recording streams
//! to the store while the call is up instead of being held in memory until the hang-up.
//!
//! [`RecordingStore`] is the object-store seam of `sutra-objectstore` (put / get / bucket
//! create, the same [`StoreLocation`], [`StoredObject`] and [`ObjectStoreError`]) plus what a
//! streamed, crash-recoverable write needs: multipart uploads (create, upload a part, complete,
//! abort, list the uploads in progress and their parts), a ranged read, and delete.
//!
//! - [`S3RecordingStore`] — any S3-compatible store, signed with `sutra_objectstore::sigv4`.
//! - [`MemoryRecordingStore`] — in process, for tests and `memory://` deployments.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sutra_objectstore::sigv4::{self, Credentials, SigningRequest};
pub use sutra_objectstore::{
    sha256_hex, valid_key, BoxFuture, ObjectStoreError, StoreLocation, StoredObject,
};

/// One uploaded part of a multipart upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedPart {
    pub number: u32,
    pub etag: String,
    pub size: u64,
}

/// An object store that takes multipart uploads. Every key is relative to the store's prefix.
pub trait RecordingStore: Send + Sync {
    fn location(&self) -> &StoreLocation;

    /// The full object key `key` is stored under (the prefix joined with `key`).
    fn full_key(&self, key: &str) -> String {
        self.location().full_key(key)
    }

    /// The URI an object at `key` is reported under.
    fn uri_of(&self, key: &str) -> String {
        self.location().uri_of(key)
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        body: Vec<u8>,
        content_type: &'a str,
    ) -> BoxFuture<'a, Result<StoredObject, ObjectStoreError>>;

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, ObjectStoreError>>;

    /// At most `len` bytes of the object at `key` from `start` (`None` when there is no object;
    /// fewer bytes, or none, past its end).
    fn get_range<'a>(
        &'a self,
        key: &'a str,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, ObjectStoreError>>;

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), ObjectStoreError>>;

    /// Create the bucket when it does not exist.
    fn ensure_bucket(&self) -> BoxFuture<'_, Result<(), ObjectStoreError>>;

    /// Start a multipart upload of `key`; answers its upload id.
    fn create_upload<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
    ) -> BoxFuture<'a, Result<String, ObjectStoreError>>;

    /// Upload part `number` (from 1); answers its ETag.
    fn upload_part<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
        number: u32,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<String, ObjectStoreError>>;

    /// Assemble `parts` (in order) into the object.
    fn complete_upload<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
        parts: &'a [UploadedPart],
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>>;

    fn abort_upload<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>>;

    /// The keys (relative to the store's prefix) of the objects whose key starts with `prefix`.
    /// (Uploads in progress are found through their markers, not ListMultipartUploads, which
    /// some S3-compatible stores answer only for an exact key.)
    fn list_keys<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, ObjectStoreError>>;

    /// The parts uploaded so far, by number.
    fn list_parts<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<UploadedPart>, ObjectStoreError>>;
}

fn check_key(full: &str) -> Result<(), ObjectStoreError> {
    if valid_key(full) {
        Ok(())
    } else {
        Err(ObjectStoreError::Config(format!(
            "'{full}' is not an object key"
        )))
    }
}

// ---- in memory ---------------------------------------------------------------------------------

#[derive(Default)]
struct MemoryState {
    /// Full key → (body, content type).
    objects: BTreeMap<String, (Vec<u8>, String)>,
    /// Upload id → (full key, parts by number).
    uploads: BTreeMap<String, (String, BTreeMap<u32, Vec<u8>>)>,
    next_upload: u64,
}

/// An in-process store with multipart uploads. Clones share their objects, so a test can hand
/// one clone to a recorder that "crashes" and another to the recorder that recovers.
#[derive(Clone)]
pub struct MemoryRecordingStore {
    location: StoreLocation,
    state: Arc<Mutex<MemoryState>>,
}

impl MemoryRecordingStore {
    pub fn new(location: StoreLocation) -> MemoryRecordingStore {
        MemoryRecordingStore {
            location,
            state: Arc::new(Mutex::new(MemoryState::default())),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every stored object's full key.
    pub fn keys(&self) -> Vec<String> {
        self.state().objects.keys().cloned().collect()
    }

    /// The content type the object at `full_key` was stored with.
    pub fn content_type(&self, full_key: &str) -> Option<String> {
        self.state().objects.get(full_key).map(|(_, t)| t.clone())
    }

    /// How many uploads are in progress.
    pub fn pending_uploads(&self) -> usize {
        self.state().uploads.len()
    }
}

impl RecordingStore for MemoryRecordingStore {
    fn location(&self) -> &StoreLocation {
        &self.location
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        body: Vec<u8>,
        content_type: &'a str,
    ) -> BoxFuture<'a, Result<StoredObject, ObjectStoreError>> {
        Box::pin(async move {
            let full = self.full_key(key);
            check_key(&full)?;
            let stored = StoredObject {
                uri: self.uri_of(key),
                key: full.clone(),
                sha256: sha256_hex(&body),
                bytes: body.len() as u64,
            };
            self.state()
                .objects
                .insert(full, (body, content_type.to_string()));
            Ok(stored)
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, ObjectStoreError>> {
        Box::pin(async move {
            let full = self.full_key(key);
            Ok(self.state().objects.get(&full).map(|(b, _)| b.clone()))
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a str,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, ObjectStoreError>> {
        Box::pin(async move {
            let full = self.full_key(key);
            Ok(self.state().objects.get(&full).map(|(body, _)| {
                let from = (start as usize).min(body.len());
                let to = (start.saturating_add(len) as usize).min(body.len());
                body[from..to].to_vec()
            }))
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let full = self.full_key(key);
            self.state().objects.remove(&full);
            Ok(())
        })
    }

    fn ensure_bucket(&self) -> BoxFuture<'_, Result<(), ObjectStoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn create_upload<'a>(
        &'a self,
        key: &'a str,
        _content_type: &'a str,
    ) -> BoxFuture<'a, Result<String, ObjectStoreError>> {
        Box::pin(async move {
            let full = self.full_key(key);
            check_key(&full)?;
            let mut state = self.state();
            state.next_upload += 1;
            let id = format!("memory-upload-{}", state.next_upload);
            state.uploads.insert(id.clone(), (full, BTreeMap::new()));
            Ok(id)
        })
    }

    fn upload_part<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
        number: u32,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<String, ObjectStoreError>> {
        Box::pin(async move {
            let full = self.full_key(key);
            let mut state = self.state();
            let Some((upload_key, parts)) = state.uploads.get_mut(upload_id) else {
                return Err(ObjectStoreError::Rejected(format!(
                    "upload part: no upload {upload_id}"
                )));
            };
            if *upload_key != full {
                return Err(ObjectStoreError::Rejected(
                    "upload part: the upload is for another key".to_string(),
                ));
            }
            let etag = format!("\"{}\"", &sha256_hex(&body)[..32]);
            parts.insert(number, body);
            Ok(etag)
        })
    }

    fn complete_upload<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
        parts: &'a [UploadedPart],
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let full = self.full_key(key);
            let mut state = self.state();
            let Some((upload_key, uploaded)) = state.uploads.remove(upload_id) else {
                return Err(ObjectStoreError::Rejected(format!(
                    "complete: no upload {upload_id}"
                )));
            };
            if upload_key != full {
                return Err(ObjectStoreError::Rejected(
                    "complete: the upload is for another key".to_string(),
                ));
            }
            let mut body = Vec::new();
            for part in parts {
                let Some(bytes) = uploaded.get(&part.number) else {
                    return Err(ObjectStoreError::Rejected(format!(
                        "complete: part {} was not uploaded",
                        part.number
                    )));
                };
                body.extend_from_slice(bytes);
            }
            state.objects.insert(
                full,
                (body, crate::recording::SEALED_STREAM_CONTENT_TYPE.into()),
            );
            Ok(())
        })
    }

    fn abort_upload<'a>(
        &'a self,
        _key: &'a str,
        upload_id: &'a str,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            self.state().uploads.remove(upload_id);
            Ok(())
        })
    }

    fn list_keys<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, ObjectStoreError>> {
        Box::pin(async move {
            let base = self.location.full_key("");
            Ok(self
                .state()
                .objects
                .keys()
                .filter_map(|full| relative_key(&base, full))
                .filter(|key| key.starts_with(prefix))
                .collect())
        })
    }

    fn list_parts<'a>(
        &'a self,
        _key: &'a str,
        upload_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<UploadedPart>, ObjectStoreError>> {
        Box::pin(async move {
            let state = self.state();
            let Some((_, parts)) = state.uploads.get(upload_id) else {
                return Err(ObjectStoreError::Rejected(format!(
                    "list parts: no upload {upload_id}"
                )));
            };
            Ok(parts
                .iter()
                .map(|(n, body)| UploadedPart {
                    number: *n,
                    etag: format!("\"{}\"", &sha256_hex(body)[..32]),
                    size: body.len() as u64,
                })
                .collect())
        })
    }
}

/// `full` relative to `prefix` (the store's prefix joined with ""), when it is under it.
fn relative_key(prefix: &str, full: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(full.to_string());
    }
    full.strip_prefix(prefix).map(str::to_string)
}

// ---- S3 ----------------------------------------------------------------------------------------

/// How an [`S3RecordingStore`] is reached.
#[derive(Debug, Clone)]
pub struct S3Settings {
    pub location: StoreLocation,
    /// A self-hosted / non-AWS store's base URL (`http://store:9000`, path-style).
    pub endpoint: Option<String>,
    pub region: String,
    pub credentials: Credentials,
    /// Bound on one request, connect to last byte.
    pub timeout: Duration,
}

/// An S3-compatible store with multipart uploads (Signature Version 4).
pub struct S3RecordingStore {
    settings: S3Settings,
    client: reqwest::Client,
}

struct Target {
    host: String,
    path: String,
    base: String,
}

impl S3RecordingStore {
    pub fn new(settings: S3Settings) -> Result<S3RecordingStore, ObjectStoreError> {
        if settings.location.scheme != "s3" {
            return Err(ObjectStoreError::Config(format!(
                "an S3 store needs an s3:// location, got {}://",
                settings.location.scheme
            )));
        }
        if let Some(endpoint) = &settings.endpoint {
            let lowered = endpoint.to_ascii_lowercase();
            if !lowered.starts_with("http://") && !lowered.starts_with("https://") {
                return Err(ObjectStoreError::Config(format!(
                    "endpoint '{endpoint}' is not an http(s) URL"
                )));
            }
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(settings.timeout)
            .build()
            .map_err(|e| ObjectStoreError::Config(format!("HTTP client: {e}")))?;
        Ok(S3RecordingStore { settings, client })
    }

    /// Where a request for `full_key` (`None`: the bucket) goes.
    fn target(&self, full_key: Option<&str>) -> Target {
        let bucket = &self.settings.location.bucket;
        let path_style = self.settings.endpoint.is_some();
        let base = self
            .settings
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("https://s3.{}.amazonaws.com", self.settings.region))
            .trim_end_matches('/')
            .to_string();
        let (scheme, authority) = base.split_once("://").unwrap_or(("https", &base));
        let authority = authority.split('/').next().unwrap_or_default().to_string();
        let key_path = full_key.map(|k| format!("/{k}")).unwrap_or_default();
        let (host, path) = if path_style {
            (authority, format!("/{bucket}{key_path}"))
        } else {
            (
                format!("{bucket}.{authority}"),
                if key_path.is_empty() {
                    "/".to_string()
                } else {
                    key_path
                },
            )
        };
        Target {
            base: format!("{scheme}://{host}"),
            host,
            path,
        }
    }

    /// Send one signed request; answers (status, headers, body).
    async fn send(
        &self,
        method: &str,
        target: &Target,
        query: &[(&str, &str)],
        extra_headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<(u16, reqwest::header::HeaderMap, Vec<u8>), ObjectStoreError> {
        let payload_hash = sha256_hex(&body);
        let amz_date = sigv4::amz_date(time::OffsetDateTime::now_utc());
        let mut headers = vec![
            ("host".to_string(), target.host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(token) = &self.settings.credentials.session_token {
            headers.push(("x-amz-security-token".to_string(), token.clone()));
        }
        headers.extend(extra_headers.iter().cloned());
        let authorization = sigv4::authorization(
            &SigningRequest {
                method,
                path: &target.path,
                query,
                headers: &headers,
                payload_hash: &payload_hash,
                region: &self.settings.region,
                amz_date: &amz_date,
            },
            &self.settings.credentials,
        );
        let mut url = format!("{}{}", target.base, sigv4::uri_encode(&target.path, true));
        if !query.is_empty() {
            let encoded: Vec<String> = query
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}={}",
                        sigv4::uri_encode(k, false),
                        sigv4::uri_encode(v, false)
                    )
                })
                .collect();
            url.push('?');
            url.push_str(&encoded.join("&"));
        }
        let method_value: reqwest::Method = method
            .parse()
            .map_err(|_| ObjectStoreError::Config(format!("bad method {method}")))?;
        let mut request = self
            .client
            .request(method_value, &url)
            .header("authorization", authorization);
        for (name, value) in headers.iter().filter(|(n, _)| n != "host") {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .body(body)
            .send()
            .await
            // The error is reported without its URL: a store URL is not a secret, but a request
            // line never goes into a log line here.
            .map_err(|e| ObjectStoreError::Unavailable(e.without_url().to_string()))?;
        let status = response.status().as_u16();
        let response_headers = response.headers().clone();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ObjectStoreError::Unavailable(e.without_url().to_string()))?;
        Ok((status, response_headers, bytes.to_vec()))
    }

    fn failure(operation: &str, status: u16, body: &[u8]) -> ObjectStoreError {
        let text = String::from_utf8_lossy(body);
        let code: String = tag_text(&text, "Code")
            .unwrap_or_default()
            .chars()
            .take(80)
            .collect();
        let message = format!("{operation}: HTTP {status} {code}")
            .trim()
            .to_string();
        if status >= 500 || status == 429 || status == 408 {
            ObjectStoreError::Unavailable(message)
        } else {
            ObjectStoreError::Rejected(message)
        }
    }

    fn key_target(&self, key: &str) -> Result<(String, Target), ObjectStoreError> {
        let full = self.full_key(key);
        check_key(&full)?;
        let target = self.target(Some(&full));
        Ok((full, target))
    }
}

/// The text of the first `<name>…</name>` in `xml`, entity-decoded.
pub fn tag_text(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(&xml[start..end]))
}

/// Every `<name>…</name>` block of `xml`.
pub fn tag_blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find(&open) {
        let after = &rest[at + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#34;", "\"")
        .replace("&amp;", "&")
}

impl RecordingStore for S3RecordingStore {
    fn location(&self) -> &StoreLocation {
        &self.settings.location
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        body: Vec<u8>,
        content_type: &'a str,
    ) -> BoxFuture<'a, Result<StoredObject, ObjectStoreError>> {
        Box::pin(async move {
            let (full, target) = self.key_target(key)?;
            let sha256 = sha256_hex(&body);
            let bytes = body.len() as u64;
            let headers = [("content-type".to_string(), content_type.to_string())];
            let (status, _, answer) = self.send("PUT", &target, &[], &headers, body).await?;
            if !(200..300).contains(&status) {
                return Err(Self::failure("put", status, &answer));
            }
            Ok(StoredObject {
                uri: self.uri_of(key),
                key: full,
                sha256,
                bytes,
            })
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, ObjectStoreError>> {
        Box::pin(async move {
            let (_, target) = self.key_target(key)?;
            let (status, _, answer) = self.send("GET", &target, &[], &[], Vec::new()).await?;
            match status {
                200..=299 => Ok(Some(answer)),
                404 => Ok(None),
                _ => Err(Self::failure("get", status, &answer)),
            }
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a str,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, ObjectStoreError>> {
        Box::pin(async move {
            if len == 0 {
                return Ok(Some(Vec::new()));
            }
            let (_, target) = self.key_target(key)?;
            let range = [(
                "range".to_string(),
                format!("bytes={start}-{}", start + len - 1),
            )];
            let (status, _, answer) = self.send("GET", &target, &[], &range, Vec::new()).await?;
            match status {
                200..=299 => Ok(Some(answer)),
                404 => Ok(None),
                // Past the end of the object.
                416 => Ok(Some(Vec::new())),
                _ => Err(Self::failure("get range", status, &answer)),
            }
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let (_, target) = self.key_target(key)?;
            let (status, _, answer) = self.send("DELETE", &target, &[], &[], Vec::new()).await?;
            if (200..300).contains(&status) || status == 404 {
                Ok(())
            } else {
                Err(Self::failure("delete", status, &answer))
            }
        })
    }

    fn ensure_bucket(&self) -> BoxFuture<'_, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let target = self.target(None);
            let body = if self.settings.region == "us-east-1" {
                Vec::new()
            } else {
                format!(
                    "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                     <LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                    self.settings.region
                )
                .into_bytes()
            };
            let (status, _, answer) = self.send("PUT", &target, &[], &[], body).await?;
            let text = String::from_utf8_lossy(&answer);
            if (200..300).contains(&status) || text.contains("BucketAlreadyOwnedByYou") {
                Ok(())
            } else {
                Err(Self::failure("create bucket", status, &answer))
            }
        })
    }

    fn create_upload<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
    ) -> BoxFuture<'a, Result<String, ObjectStoreError>> {
        Box::pin(async move {
            let (_, target) = self.key_target(key)?;
            let headers = [("content-type".to_string(), content_type.to_string())];
            let (status, _, answer) = self
                .send("POST", &target, &[("uploads", "")], &headers, Vec::new())
                .await?;
            if !(200..300).contains(&status) {
                return Err(Self::failure("create upload", status, &answer));
            }
            tag_text(&String::from_utf8_lossy(&answer), "UploadId")
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    ObjectStoreError::Rejected("create upload: no UploadId in the answer".into())
                })
        })
    }

    fn upload_part<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
        number: u32,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<String, ObjectStoreError>> {
        Box::pin(async move {
            let (_, target) = self.key_target(key)?;
            let number_text = number.to_string();
            let query = [
                ("partNumber", number_text.as_str()),
                ("uploadId", upload_id),
            ];
            let (status, headers, answer) = self.send("PUT", &target, &query, &[], body).await?;
            if !(200..300).contains(&status) {
                return Err(Self::failure("upload part", status, &answer));
            }
            headers
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| ObjectStoreError::Rejected("upload part: no ETag".into()))
        })
    }

    fn complete_upload<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
        parts: &'a [UploadedPart],
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let (_, target) = self.key_target(key)?;
            let mut xml = String::from("<CompleteMultipartUpload>");
            for part in parts {
                let etag = part
                    .etag
                    .replace('&', "&amp;")
                    .replace('"', "&quot;")
                    .replace('<', "&lt;");
                xml.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{etag}</ETag></Part>",
                    part.number
                ));
            }
            xml.push_str("</CompleteMultipartUpload>");
            let headers = [("content-type".to_string(), "application/xml".to_string())];
            let (status, _, answer) = self
                .send(
                    "POST",
                    &target,
                    &[("uploadId", upload_id)],
                    &headers,
                    xml.into_bytes(),
                )
                .await?;
            // A completion can fail with 200 and an <Error> body.
            let text = String::from_utf8_lossy(&answer);
            if !(200..300).contains(&status) || text.contains("<Error>") {
                return Err(Self::failure("complete upload", status.max(500), &answer));
            }
            Ok(())
        })
    }

    fn abort_upload<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let (_, target) = self.key_target(key)?;
            let (status, _, answer) = self
                .send(
                    "DELETE",
                    &target,
                    &[("uploadId", upload_id)],
                    &[],
                    Vec::new(),
                )
                .await?;
            if (200..300).contains(&status) || status == 404 {
                Ok(())
            } else {
                Err(Self::failure("abort upload", status, &answer))
            }
        })
    }

    fn list_keys<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, ObjectStoreError>> {
        Box::pin(async move {
            let target = self.target(None);
            let base = self.location().full_key("");
            let full_prefix = format!("{base}{prefix}");
            let mut out = Vec::new();
            let mut token = String::new();
            for _ in 0..1000 {
                let mut query: Vec<(&str, &str)> =
                    vec![("list-type", "2"), ("prefix", &full_prefix)];
                if !token.is_empty() {
                    query.push(("continuation-token", &token));
                }
                let (status, _, answer) =
                    self.send("GET", &target, &query, &[], Vec::new()).await?;
                if !(200..300).contains(&status) {
                    return Err(Self::failure("list objects", status, &answer));
                }
                let text = String::from_utf8_lossy(&answer).to_string();
                for block in tag_blocks(&text, "Contents") {
                    if let Some(key) = tag_text(block, "Key").and_then(|k| relative_key(&base, &k))
                    {
                        out.push(key);
                    }
                }
                if tag_text(&text, "IsTruncated").as_deref() != Some("true") {
                    break;
                }
                token = tag_text(&text, "NextContinuationToken").unwrap_or_default();
                if token.is_empty() {
                    break;
                }
            }
            Ok(out)
        })
    }

    fn list_parts<'a>(
        &'a self,
        key: &'a str,
        upload_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<UploadedPart>, ObjectStoreError>> {
        Box::pin(async move {
            let (_, target) = self.key_target(key)?;
            let mut out = Vec::new();
            let mut marker = String::new();
            for _ in 0..100 {
                let mut query: Vec<(&str, &str)> = vec![("uploadId", upload_id)];
                if !marker.is_empty() {
                    query.push(("part-number-marker", &marker));
                }
                let (status, _, answer) =
                    self.send("GET", &target, &query, &[], Vec::new()).await?;
                if !(200..300).contains(&status) {
                    return Err(Self::failure("list parts", status, &answer));
                }
                let text = String::from_utf8_lossy(&answer).to_string();
                for block in tag_blocks(&text, "Part") {
                    let number = tag_text(block, "PartNumber").and_then(|n| n.parse().ok());
                    let size = tag_text(block, "Size").and_then(|n| n.parse().ok());
                    if let (Some(number), Some(etag), Some(size)) =
                        (number, tag_text(block, "ETag"), size)
                    {
                        out.push(UploadedPart { number, etag, size });
                    }
                }
                if tag_text(&text, "IsTruncated").as_deref() != Some("true") {
                    break;
                }
                marker = tag_text(&text, "NextPartNumberMarker").unwrap_or_default();
                if marker.is_empty() {
                    break;
                }
            }
            out.sort_by_key(|p| p.number);
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_xml_answers() {
        let xml = "<ListMultipartUploadsResult><IsTruncated>false</IsTruncated>\
            <Upload><Key>calls/a.wav.sealed</Key><UploadId>u-1</UploadId></Upload>\
            <Upload><Key>calls/b&amp;c</Key><UploadId>u-2</UploadId></Upload></ListMultipartUploadsResult>";
        let blocks = tag_blocks(xml, "Upload");
        assert_eq!(blocks.len(), 2);
        assert_eq!(tag_text(blocks[1], "Key").unwrap(), "calls/b&c");
        assert_eq!(tag_text(xml, "IsTruncated").unwrap(), "false");
        assert_eq!(relative_key("calls/", "calls/a.wav"), Some("a.wav".into()));
        assert_eq!(relative_key("calls/", "other/a.wav"), None);
    }

    #[test]
    fn addresses_path_style_with_an_endpoint() {
        let store = S3RecordingStore::new(S3Settings {
            location: StoreLocation::parse("s3://recordings/calls").unwrap(),
            endpoint: Some("http://store:9000/".into()),
            region: "us-east-1".into(),
            credentials: Credentials {
                access_key: "a".into(),
                secret_key: "b".into(),
                session_token: None,
            },
            timeout: Duration::from_secs(5),
        })
        .unwrap();
        let t = store.target(Some("calls/x.wav"));
        assert_eq!(
            (t.base.as_str(), t.host.as_str(), t.path.as_str()),
            ("http://store:9000", "store:9000", "/recordings/calls/x.wav")
        );
        assert_eq!(store.target(None).path, "/recordings");
    }
}
