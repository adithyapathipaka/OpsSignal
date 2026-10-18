//! Cloud config sources, compiled in by the `aws`, `gcp` and `azure`
//! cargo features. Each cloud's code lives in its own module and only
//! ever ships in that cloud's build (the `opssignal[aws]` wheel, a CLI
//! built with `--features aws`, and so on).
//!
//! Everything runs on the crate's one reqwest 0.12 + rustls stack.
//! Object stores go through `object_store`, whose credential chains are
//! also reused to authenticate the other services of each cloud.

use crate::signal::SignalError;

#[cfg(feature = "aws")]
pub(crate) mod aws;
#[cfg(feature = "azure")]
pub(crate) mod azure;
#[cfg(feature = "gcp")]
pub(crate) mod gcp;

pub(crate) use crate::source::FETCH_TIMEOUT;

/// Lets object_store run its requests (including credential and token
/// lookups) on the crate's existing reqwest 0.12 client. Config files
/// are small, so bodies are buffered rather than streamed.
#[derive(Debug)]
pub(crate) struct ReqwestConnector;

impl object_store::client::HttpConnector for ReqwestConnector {
    fn connect(
        &self,
        options: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        // object_store refuses plain http unless allowed (it allows it
        // for metadata and token endpoints, or when AWS_ALLOW_HTTP and
        // friends are set); keep that rule with our own client.
        let allow_http = options
            .get_config_value(&object_store::ClientConfigKey::AllowHttp)
            .is_some_and(|v| v == "true");
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .https_only(!allow_http)
            .build()
            .map_err(|e| object_store::Error::Generic {
                store: "opssignal",
                source: Box::new(e),
            })?;
        Ok(object_store::client::HttpClient::new(ReqwestService(
            client,
        )))
    }
}

#[derive(Debug)]
struct ReqwestService(reqwest::Client);

#[async_trait::async_trait]
impl object_store::client::HttpService for ReqwestService {
    async fn call(
        &self,
        req: object_store::client::HttpRequest,
    ) -> Result<object_store::client::HttpResponse, object_store::client::HttpError> {
        use object_store::client::{HttpError, HttpErrorKind, HttpResponseBody};

        let (parts, body) = req.into_parts();
        let body = match body.as_bytes() {
            Some(bytes) => bytes.clone(),
            None if body.is_empty() => bytes::Bytes::new(),
            None => {
                return Err(HttpError::new_boxed(
                    HttpErrorKind::Request,
                    "streaming request bodies are not supported".into(),
                ))
            }
        };
        let url = reqwest::Url::parse(&parts.uri.to_string())
            .map_err(|e| HttpError::new(HttpErrorKind::Request, e))?;
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = parts.headers;
        *request.body_mut() = Some(body.into());

        let kind = |e: &reqwest::Error| {
            if e.is_timeout() {
                HttpErrorKind::Timeout
            } else if e.is_connect() {
                HttpErrorKind::Connect
            } else {
                HttpErrorKind::Request
            }
        };
        let resp = self
            .0
            .execute(request)
            .await
            .map_err(|e| HttpError::new(kind(&e), e))?;
        let mut out = http::Response::builder().status(resp.status());
        if let Some(headers) = out.headers_mut() {
            *headers = resp.headers().clone();
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| HttpError::new(HttpErrorKind::Decode, e))?;
        out.body(HttpResponseBody::from(bytes))
            .map_err(|e| HttpError::new(HttpErrorKind::Interrupted, e))
    }
}

pub(crate) async fn fetch_object(
    store: impl object_store::ObjectStore,
    path: &str,
) -> Result<String, SignalError> {
    use object_store::ObjectStoreExt;
    let get = async {
        let bytes = store
            .get(&object_store::path::Path::from(path))
            .await?
            .bytes()
            .await?;
        Ok::<_, object_store::Error>(bytes)
    };
    let bytes = tokio::time::timeout(FETCH_TIMEOUT, get)
        .await
        .map_err(|_| config_err(format!("timed out after {FETCH_TIMEOUT:?}")))?
        .map_err(|e| config_err(e.to_string()))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| config_err("config is not valid UTF-8"))
}

// Shared by the cloud modules; a build with only some clouds leaves
// some of these unused.
#[cfg_attr(not(all(feature = "aws", feature = "azure")), allow(dead_code))]
/// A plain reqwest client with the fetch timeout.
pub(crate) fn client() -> Result<reqwest::Client, SignalError> {
    reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| config_err(e.to_string()))
}

// Shared by the cloud modules; a build with only some clouds leaves
// some of these unused.
#[cfg_attr(not(all(feature = "aws", feature = "azure")), allow(dead_code))]
/// Sends a (signed) object_store request on reqwest and buffers the
/// response.
pub(crate) async fn send(
    req: object_store::client::HttpRequest,
) -> Result<
    (
        reqwest::StatusCode,
        reqwest::header::HeaderMap,
        bytes::Bytes,
    ),
    SignalError,
> {
    let (parts, body) = req.into_parts();
    let url = reqwest::Url::parse(&parts.uri.to_string()).map_err(|e| config_err(e.to_string()))?;
    let mut request = reqwest::Request::new(parts.method, url);
    *request.headers_mut() = parts.headers;
    *request.body_mut() = Some(body.as_bytes().cloned().unwrap_or_default().into());
    let resp = client()?
        .execute(request)
        .await
        .map_err(|e| config_err(e.to_string()))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.map_err(|e| config_err(e.to_string()))?;
    Ok((status, headers, bytes))
}

// Shared by the cloud modules; a build with only some clouds leaves
// some of these unused.
#[cfg_attr(not(all(feature = "aws", feature = "azure")), allow(dead_code))]
/// An error for a non-success response, with the start of its body,
/// which is where every cloud puts the reason.
pub(crate) fn status_err(status: reqwest::StatusCode, body: &[u8]) -> SignalError {
    let body = String::from_utf8_lossy(body);
    let body: String = body.trim().chars().take(300).collect();
    if body.is_empty() {
        config_err(format!("server returned {status}"))
    } else {
        config_err(format!("server returned {status}: {body}"))
    }
}

// Shared by the cloud modules; a build with only some clouds leaves
// some of these unused.
#[cfg_attr(not(all(feature = "aws", feature = "azure")), allow(dead_code))]
/// An endpoint override must be https://, or http:// to a loopback
/// host (for local emulators and tests).
pub(crate) fn check_endpoint(url: &str) -> Result<(), SignalError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| config_err(format!("{url}: {e}")))?;
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback) {
        Ok(())
    } else {
        Err(config_err(format!(
            "{url}: endpoints must use https:// (plain http:// only for localhost)"
        )))
    }
}

// Shared by the cloud modules; a build with only some clouds leaves
// some of these unused.
#[cfg_attr(not(all(feature = "aws", feature = "azure")), allow(dead_code))]
pub(crate) fn utf8(bytes: &[u8]) -> Result<String, SignalError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| config_err("config is not valid UTF-8"))
}

pub(crate) fn config_err(msg: impl Into<String>) -> SignalError {
    SignalError::Config(msg.into())
}
