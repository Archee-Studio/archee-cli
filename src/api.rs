//! HTTP client for the Archee backend API; the only way the CLI reaches the server.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cli::PRODUCT_NAME;
use crate::models::{
    AssignmentListing, ClaimedAssignment, ExchangeResponse, LoginPoll, LoginStarted, OperationAck,
    OperationEnvelope, OperationResult, OutpostView, RegistrationIssued, User,
};
use crate::output::{CliError, EXIT_AUTH, EXIT_USAGE};

pub(crate) const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A failed call, keeping what the worker's retry rules and the user's next step depend on.
#[derive(Debug)]
pub(crate) enum ApiError {
    Status { status: u16, detail: String },
    Network(String),
    Decode(String),
}

impl From<ApiError> for CliError {
    fn from(error: ApiError) -> Self {
        match error {
            ApiError::Status { status: 401, .. } => {
                CliError::new("unauthorized", "you are not signed in")
                    .next("archee login")
                    .exit(EXIT_AUTH)
            }
            ApiError::Status { status, detail } => {
                CliError::new("api", format!("the server answered {status}: {detail}"))
            }
            ApiError::Network(detail) => CliError::new(
                "network",
                format!("could not reach {PRODUCT_NAME}: {detail}"),
            )
            .next("check the connection and ARCHEE_URL, then retry"),
            ApiError::Decode(detail) => {
                CliError::new("api", format!("unexpected answer: {detail}"))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Client {
    base: String,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct Detail {
    detail: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct LeaseBody<'a> {
    lease_token: &'a str,
    fence_token: i64,
}

#[derive(Debug, Serialize)]
struct ResultBody<'a> {
    status: &'a str,
    body: &'a OperationResult,
    lease_token: &'a str,
    fence_token: i64,
}

fn empty() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn os_arch() -> String {
    format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A token may travel only over TLS, or to this machine.
fn secure_url(base: &str) -> bool {
    let Some(rest) = base.strip_prefix("http://") else {
        return base.starts_with("https://");
    };
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.bytes().all(|b| b.is_ascii_digit()) {
            name
        } else {
            host
        }
    });
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

impl Client {
    pub(crate) fn new(base: &str, token: Option<&str>) -> Result<Self, CliError> {
        if !secure_url(base) {
            return Err(CliError::new(
                "usage",
                format!("{base} is not https, so a token sent there could be read on the way"),
            )
            .next("use an https:// URL; plain http works only for localhost")
            .exit(EXIT_USAGE));
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Archee-CLI-Version",
            HeaderValue::from_static(CLI_VERSION),
        );
        if let Some(token) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| CliError::new("usage", "the token has invalid characters"))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| {
                CliError::new("internal", format!("could not start the HTTP client: {e}"))
            })?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            http,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http.request(method, format!("{}{path}", self.base))
    }

    async fn send(&self, builder: RequestBuilder) -> Result<Response, ApiError> {
        let response = builder
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = response.status();
        if !status.is_client_error() && !status.is_server_error() {
            return Ok(response);
        }
        let text = response.text().await.unwrap_or_default();
        let detail = match serde_json::from_str::<Detail>(&text) {
            Ok(Detail {
                detail: serde_json::Value::String(detail),
            }) => detail,
            Ok(Detail { detail }) => detail.to_string(),
            Err(_) => text,
        };
        Err(ApiError::Status {
            status: status.as_u16(),
            detail,
        })
    }

    async fn json<T: DeserializeOwned>(&self, builder: RequestBuilder) -> Result<T, ApiError> {
        let response = self.send(builder.timeout(REQUEST_TIMEOUT)).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
    }

    async fn post<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        self.json(self.request(Method::POST, path).json(body)).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        self.json(self.request(Method::GET, path)).await
    }

    async fn post_no_reply<B: Serialize>(&self, path: &str, body: &B) -> Result<(), ApiError> {
        let builder = self
            .request(Method::POST, path)
            .json(body)
            .timeout(REQUEST_TIMEOUT);
        self.send(builder).await.map(|_| ())
    }

    // -- user commands --------------------------------------------------------------------------

    pub(crate) async fn login_start(&self) -> Result<LoginStarted, ApiError> {
        self.post("/api/auth/cli/start", &empty()).await
    }

    pub(crate) async fn login_poll(&self, device_code: &str) -> Result<LoginPoll, ApiError> {
        self.post(
            "/api/auth/cli/poll",
            &serde_json::json!({"device_code": device_code}),
        )
        .await
    }

    pub(crate) async fn logout(&self) -> Result<(), ApiError> {
        self.post_no_reply("/api/auth/logout", &empty()).await
    }

    pub(crate) async fn me(&self) -> Result<User, ApiError> {
        self.get("/api/me").await
    }

    // -- outposts -------------------------------------------------------------------------------

    pub(crate) async fn create_outpost(&self, name: &str) -> Result<OutpostView, ApiError> {
        self.post("/api/outposts", &serde_json::json!({"name": name}))
            .await
    }

    pub(crate) async fn list_outposts(&self) -> Result<Vec<OutpostView>, ApiError> {
        self.get("/api/outposts").await
    }

    pub(crate) async fn issue_registration(
        &self,
        id: &str,
    ) -> Result<RegistrationIssued, ApiError> {
        self.post(&format!("/api/outposts/{id}/registrations"), &empty())
            .await
    }

    // -- worker protocol ------------------------------------------------------------------------

    pub(crate) async fn exchange(
        &self,
        reference: &str,
        instance_id: &str,
        workdir_id: &str,
    ) -> Result<ExchangeResponse, ApiError> {
        let body = serde_json::json!({
            "reference": reference, "instance_id": instance_id, "workdir_id": workdir_id,
            "os_arch": os_arch(), "cli_version": CLI_VERSION,
        });
        self.post("/api/outpost-workers/exchange", &body).await
    }

    pub(crate) async fn register(&self, workdir_id: &str) -> Result<(), ApiError> {
        let body = serde_json::json!({"workdir_id": workdir_id, "os_arch": os_arch(), "cli_version": CLI_VERSION});
        self.post_no_reply("/api/outpost-workers/register", &body)
            .await
    }

    pub(crate) async fn heartbeat(&self) -> Result<(), ApiError> {
        self.post_no_reply(
            "/api/outpost-workers/heartbeat",
            &serde_json::json!({"status": "online"}),
        )
        .await
    }

    pub(crate) async fn assignments(&self) -> Result<Vec<AssignmentListing>, ApiError> {
        self.get("/api/outpost-workers/assignments").await
    }

    pub(crate) async fn claim(&self, id: &str) -> Result<ClaimedAssignment, ApiError> {
        self.post(
            &format!("/api/outpost-workers/assignments/{id}/claim"),
            &empty(),
        )
        .await
    }

    pub(crate) async fn feed(
        &self,
        lease: &ClaimedAssignment,
        after: i64,
        wait: u32,
    ) -> Result<Vec<OperationEnvelope>, ApiError> {
        let builder = self
            .request(
                Method::GET,
                &format!(
                    "/api/outpost-workers/assignments/{}/operations",
                    lease.assignment_id
                ),
            )
            .query(&[
                ("lease_token", lease.lease_token.as_str()),
                ("fence_token", &lease.fence_token.to_string()),
                ("after", &after.to_string()),
                ("wait", &wait.to_string()),
            ]);
        self.json(builder).await
    }

    pub(crate) async fn renew(&self, lease: &ClaimedAssignment) -> Result<(), ApiError> {
        let body = LeaseBody {
            lease_token: &lease.lease_token,
            fence_token: lease.fence_token,
        };
        self.post_no_reply(
            &format!(
                "/api/outpost-workers/assignments/{}/renew",
                lease.assignment_id
            ),
            &body,
        )
        .await
    }

    pub(crate) async fn start(
        &self,
        lease: &ClaimedAssignment,
        operation_id: &str,
    ) -> Result<(), ApiError> {
        let body = LeaseBody {
            lease_token: &lease.lease_token,
            fence_token: lease.fence_token,
        };
        let path = format!(
            "/api/outpost-workers/assignments/{}/operations/{operation_id}/start",
            lease.assignment_id
        );
        self.post_no_reply(&path, &body).await
    }

    pub(crate) async fn result(
        &self,
        lease: &ClaimedAssignment,
        operation_id: &str,
        status: &str,
        result: &OperationResult,
    ) -> Result<OperationAck, ApiError> {
        let body = ResultBody {
            status,
            body: result,
            lease_token: &lease.lease_token,
            fence_token: lease.fence_token,
        };
        let path = format!(
            "/api/outpost-workers/assignments/{}/operations/{operation_id}/result",
            lease.assignment_id
        );
        self.post(&path, &body).await
    }

    pub(crate) async fn release(&self, lease: &ClaimedAssignment) -> Result<(), ApiError> {
        let path = format!(
            "/api/outpost-workers/assignments/{}/release",
            lease.assignment_id
        );
        self.post_no_reply(&path, &empty()).await
    }
}

#[cfg(test)]
mod tests {
    use super::secure_url;

    #[test]
    fn a_token_travels_only_over_tls_or_to_this_machine() {
        for good in [
            "https://app.archee.studio",
            "http://localhost:8000",
            "http://127.0.0.1:9/x",
            "http://[::1]:8000",
        ] {
            assert!(secure_url(good), "{good}");
        }
        for bad in [
            "http://app.archee.studio",
            "http://localhost.evil.test",
            "http://localhost@evil.test",
            "http://127.0.0.1:80@evil.test",
            "ftp://x",
            "app.archee.studio",
        ] {
            assert!(!secure_url(bad), "{bad}");
        }
    }
}
