//! LFS Batch API client — HTTPS basic transfer adapter.
//!
//! Spec: <https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md>
//! and <https://github.com/git-lfs/git-lfs/blob/main/docs/api/basic-transfers.md>
//!
//! The endpoint is derived from the git remote URL by appending `/info/lfs`.
//! The client issues a single batch request listing all needed objects, then
//! performs per-object GET (download) or PUT (upload) using the URLs and
//! headers returned by the server.

use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::creds::CredentialProvider;
use crate::store::Store;

const MEDIA_TYPE: &str = "application/vnd.git-lfs+json";
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

pub struct ObjectSpec {
    pub oid: [u8; 32],
    pub size: u64,
}

impl ObjectSpec {
    fn oid_hex(&self) -> String {
        self.oid.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Default)]
pub struct DownloadReport {
    pub succeeded: Vec<[u8; 32]>,
    pub failed: Vec<(String, String)>, // (oid_hex, reason)
}

#[derive(Default)]
pub struct UploadReport {
    pub succeeded: Vec<[u8; 32]>,
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, Error)]
pub enum BatchError {
    #[error("invalid remote url {0}: {1}")]
    InvalidUrl(String, String),
    #[error("http error: {0}")]
    Http(String),
    #[error("malformed batch response: {0}")]
    MalformedResponse(String),
    #[error("unsupported transfer adapter: {0} (maw-lfs only supports 'basic')")]
    UnsupportedTransfer(String),
    #[error("authentication failed for {0}")]
    AuthFailed(String),
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },
    #[error("no credentials for {0}")]
    NoCreds(String),
    #[error("store error: {0}")]
    Store(#[from] crate::store::StoreError),
}

pub struct BatchClient {
    endpoint: String, // .../info/lfs/objects/batch
    host: String,
    http: reqwest::blocking::Client,
    creds: CredentialProvider,
}

impl BatchClient {
    /// Build a client for the given git remote URL.
    /// Appends `/info/lfs` to the remote URL to form the LFS server base.
    pub fn new(remote_url: &str, creds: CredentialProvider) -> Result<Self, BatchError> {
        let base = derive_lfs_base(remote_url)?;
        let endpoint = format!("{base}/objects/batch");
        let host = extract_host(&endpoint)?;
        let http = reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("maw-lfs/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BatchError::Http(e.to_string()))?;
        Ok(Self {
            endpoint,
            host,
            http,
            creds,
        })
    }

    /// Download all `objects` into `store`.
    pub fn download(
        &mut self,
        objects: &[ObjectSpec],
        store: &Store,
    ) -> Result<DownloadReport, BatchError> {
        if objects.is_empty() {
            return Ok(DownloadReport::default());
        }
        let resp = self.batch("download", objects)?;
        let mut report = DownloadReport::default();
        for obj in resp.objects {
            let Ok(oid_bytes) = hex_to_oid(&obj.oid) else {
                report
                    .failed
                    .push((obj.oid.clone(), "bad oid hex".to_owned()));
                continue;
            };
            if let Some(err) = obj.error {
                report.failed.push((obj.oid, err.message));
                continue;
            }
            let Some(actions) = obj.actions else {
                // No actions typically means "already present on server" for
                // upload; for download it means the server couldn't serve it.
                report
                    .failed
                    .push((obj.oid, "server returned no download action".to_owned()));
                continue;
            };
            let Some(dl) = actions.download else {
                report
                    .failed
                    .push((obj.oid, "no download action".to_owned()));
                continue;
            };
            match self.fetch_and_store(&dl, &oid_bytes, obj.size, store) {
                Ok(()) => report.succeeded.push(oid_bytes),
                Err(e) => report.failed.push((obj.oid, e.to_string())),
            }
        }
        Ok(report)
    }

    /// Upload all `objects` from `store` to the server.
    pub fn upload(
        &mut self,
        objects: &[ObjectSpec],
        store: &Store,
    ) -> Result<UploadReport, BatchError> {
        if objects.is_empty() {
            return Ok(UploadReport::default());
        }
        let resp = self.batch("upload", objects)?;
        let mut report = UploadReport::default();
        for obj in resp.objects {
            let Ok(oid_bytes) = hex_to_oid(&obj.oid) else {
                report
                    .failed
                    .push((obj.oid.clone(), "bad oid hex".to_owned()));
                continue;
            };
            if let Some(err) = obj.error {
                report.failed.push((obj.oid, err.message));
                continue;
            }
            let Some(actions) = obj.actions else {
                // Server says it already has this object — treat as success.
                report.succeeded.push(oid_bytes);
                continue;
            };
            let Some(up) = actions.upload else {
                // No upload action and no error → already present.
                report.succeeded.push(oid_bytes);
                continue;
            };
            match self.put_and_verify(&up, actions.verify.as_ref(), &oid_bytes, obj.size, store) {
                Ok(()) => report.succeeded.push(oid_bytes),
                Err(e) => report.failed.push((obj.oid, e.to_string())),
            }
        }
        Ok(report)
    }

    fn batch(
        &mut self,
        operation: &str,
        objects: &[ObjectSpec],
    ) -> Result<BatchResponse, BatchError> {
        let body = BatchRequest {
            operation: operation.to_owned(),
            transfers: vec!["basic".to_owned()],
            hash_algo: "sha256".to_owned(),
            objects: objects
                .iter()
                .map(|o| BatchObjectReq {
                    oid: o.oid_hex(),
                    size: o.size,
                })
                .collect(),
        };

        // Try up to 2 times: once without fresh creds, once after reject-and-refetch.
        for attempt in 0..2 {
            let creds = self
                .creds
                .get(&self.host)
                .map_err(|_| BatchError::NoCreds(self.host.clone()))?;
            let resp = self
                .http
                .post(&self.endpoint)
                .header("Accept", MEDIA_TYPE)
                .header("Content-Type", MEDIA_TYPE)
                .basic_auth(&creds.username, Some(&creds.password))
                .json(&body)
                .send()
                .map_err(|e| BatchError::Http(e.to_string()))?;
            let status = resp.status();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                if attempt == 0 {
                    self.creds.reject(&self.host);
                    continue;
                }
                return Err(BatchError::AuthFailed(self.host.clone()));
            }
            if !status.is_success() {
                let body = resp.text().unwrap_or_default();
                return Err(BatchError::Server {
                    status: status.as_u16(),
                    body,
                });
            }
            let parsed: BatchResponse = resp
                .json()
                .map_err(|e| BatchError::MalformedResponse(e.to_string()))?;
            if parsed.transfer.as_deref().unwrap_or("basic") != "basic" {
                return Err(BatchError::UnsupportedTransfer(
                    parsed.transfer.unwrap_or_default(),
                ));
            }
            return Ok(parsed);
        }
        unreachable!("loop always returns")
    }

    fn fetch_and_store(
        &self,
        action: &ActionLink,
        oid: &[u8; 32],
        size: u64,
        store: &Store,
    ) -> Result<(), BatchError> {
        let mut req = self.http.get(&action.href);
        for (k, v) in action.header.iter().flatten() {
            req = req.header(k, v);
        }
        let resp = req.send().map_err(|e| BatchError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BatchError::Server {
                status: resp.status().as_u16(),
                body: format!("GET {}", action.href),
            });
        }
        let reader = resp;
        store.insert_from_stream(oid, size, reader)?;
        Ok(())
    }

    fn put_and_verify(
        &self,
        upload: &ActionLink,
        verify: Option<&ActionLink>,
        oid: &[u8; 32],
        size: u64,
        store: &Store,
    ) -> Result<(), BatchError> {
        let reader = store
            .open_object(oid)?
            .ok_or_else(|| BatchError::Http(format!("object missing from local store")))?;
        let mut req = self.http.put(&upload.href);
        for (k, v) in upload.header.iter().flatten() {
            req = req.header(k, v);
        }
        let body = reqwest::blocking::Body::sized(ReaderBody(reader), size);
        let resp = req
            .body(body)
            .send()
            .map_err(|e| BatchError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BatchError::Server {
                status: resp.status().as_u16(),
                body: format!("PUT {}", upload.href),
            });
        }
        if let Some(v) = verify {
            let mut vreq = self
                .http
                .post(&v.href)
                .header("Accept", MEDIA_TYPE)
                .header("Content-Type", MEDIA_TYPE);
            for (k, val) in v.header.iter().flatten() {
                vreq = vreq.header(k, val);
            }
            let oid_hex: String = oid.iter().map(|b| format!("{b:02x}")).collect();
            let vresp = vreq
                .json(&VerifyBody { oid: oid_hex, size })
                .send()
                .map_err(|e| BatchError::Http(e.to_string()))?;
            if !vresp.status().is_success() {
                return Err(BatchError::Server {
                    status: vresp.status().as_u16(),
                    body: format!("verify {}", v.href),
                });
            }
        }
        Ok(())
    }
}

struct ReaderBody(Box<dyn Read + Send>);

impl Read for ReaderBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

// ---- URL derivation ----

fn derive_lfs_base(remote_url: &str) -> Result<String, BatchError> {
    // Strip trailing `.git` (some servers want it kept, but most public
    // servers accept both; append `/info/lfs` either way).
    // Common scheme: https://github.com/user/repo.git → .../repo.git/info/lfs
    let trimmed = remote_url.trim_end_matches('/');
    // Reject ssh:// and git:// — we only support https:// transport.
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err(BatchError::InvalidUrl(
            remote_url.to_owned(),
            "only http(s):// remotes supported".to_owned(),
        ));
    }
    Ok(format!("{trimmed}/info/lfs"))
}

fn extract_host(url: &str) -> Result<String, BatchError> {
    // url := scheme://host[:port]/...
    let without_scheme = url
        .split_once("://")
        .map(|(_, r)| r)
        .ok_or_else(|| BatchError::InvalidUrl(url.to_owned(), "no scheme".to_owned()))?;
    let host = without_scheme.split('/').next().unwrap_or("");
    // Strip port for credential lookup (netrc convention).
    let host = host.split(':').next().unwrap_or(host);
    Ok(host.to_owned())
}

fn hex_to_oid(hex: &str) -> Result<[u8; 32], ()> {
    if hex.len() != 64 {
        return Err(());
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(out)
}

// ---- Wire types ----

#[derive(Serialize)]
struct BatchRequest {
    operation: String,
    transfers: Vec<String>,
    #[serde(rename = "hash_algo")]
    hash_algo: String,
    objects: Vec<BatchObjectReq>,
}

#[derive(Serialize)]
struct BatchObjectReq {
    oid: String,
    size: u64,
}

#[derive(Deserialize)]
struct BatchResponse {
    #[serde(default)]
    transfer: Option<String>,
    objects: Vec<BatchObjectResp>,
}

#[derive(Deserialize)]
struct BatchObjectResp {
    oid: String,
    size: u64,
    #[serde(default)]
    actions: Option<Actions>,
    #[serde(default)]
    error: Option<ObjectError>,
}

#[derive(Deserialize)]
struct Actions {
    #[serde(default)]
    download: Option<ActionLink>,
    #[serde(default)]
    upload: Option<ActionLink>,
    #[serde(default)]
    verify: Option<ActionLink>,
}

#[derive(Deserialize)]
struct ActionLink {
    href: String,
    #[serde(default)]
    header: Option<HashMap<String, String>>,
    #[serde(default)]
    #[allow(dead_code)]
    expires_at: Option<String>,
}

#[derive(Deserialize)]
struct ObjectError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

#[derive(Serialize)]
struct VerifyBody {
    oid: String,
    size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_lfs_base_https() {
        assert_eq!(
            derive_lfs_base("https://github.com/bob/repo.git").unwrap(),
            "https://github.com/bob/repo.git/info/lfs"
        );
    }

    #[test]
    fn derive_lfs_base_trailing_slash() {
        assert_eq!(
            derive_lfs_base("https://example.com/repo/").unwrap(),
            "https://example.com/repo/info/lfs"
        );
    }

    #[test]
    fn derive_lfs_base_rejects_ssh() {
        assert!(derive_lfs_base("git@github.com:bob/repo.git").is_err());
        assert!(derive_lfs_base("ssh://github.com/bob/repo.git").is_err());
    }

    #[test]
    fn extract_host_parses_port() {
        assert_eq!(
            extract_host("https://git.example.com:8443/x").unwrap(),
            "git.example.com"
        );
        assert_eq!(
            extract_host("https://github.com/x/y.git").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn hex_to_oid_round_trip() {
        let hex = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let oid = hex_to_oid(hex).unwrap();
        let back: String = oid.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(back, hex);
    }

    #[test]
    fn hex_to_oid_rejects_bad_length() {
        assert!(hex_to_oid("deadbeef").is_err());
    }

    #[test]
    fn batch_request_body_shape() {
        let body = BatchRequest {
            operation: "download".to_owned(),
            transfers: vec!["basic".to_owned()],
            hash_algo: "sha256".to_owned(),
            objects: vec![BatchObjectReq {
                oid: "abc".to_owned(),
                size: 12,
            }],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["operation"], "download");
        assert_eq!(json["transfers"][0], "basic");
        assert_eq!(json["hash_algo"], "sha256");
        assert_eq!(json["objects"][0]["oid"], "abc");
        assert_eq!(json["objects"][0]["size"], 12);
    }

    #[test]
    fn batch_response_parses() {
        let body = r#"{
            "transfer": "basic",
            "objects": [
                {
                    "oid": "deadbeef",
                    "size": 10,
                    "actions": {
                        "download": {
                            "href": "https://cdn.example/file",
                            "header": {"Authorization": "Bearer xyz"}
                        }
                    }
                },
                {
                    "oid": "cafebabe",
                    "size": 0,
                    "error": { "code": 404, "message": "not found" }
                }
            ]
        }"#;
        let parsed: BatchResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.transfer.as_deref(), Some("basic"));
        assert_eq!(parsed.objects.len(), 2);
        assert_eq!(parsed.objects[0].oid, "deadbeef");
        assert!(parsed.objects[0].actions.is_some());
        assert!(parsed.objects[0].error.is_none());
        assert!(parsed.objects[1].error.is_some());
        assert_eq!(
            parsed.objects[1].error.as_ref().unwrap().message,
            "not found"
        );
    }

    #[test]
    fn client_construction() {
        let creds = CredentialProvider::empty();
        let client = BatchClient::new("https://github.com/bob/repo.git", creds).unwrap();
        assert!(client.endpoint.ends_with("/info/lfs/objects/batch"));
        assert_eq!(client.host, "github.com");
    }
}
