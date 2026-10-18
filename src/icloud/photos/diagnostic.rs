//! Restricted transport for the missing-reference command. Reuses PhotosService.
use super::session::{DiagnosticLookupError, PhotosSession, diagnostic_bytes};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub(crate) const TRANSFER_LIMIT: usize = 1024 * 1024 * 1024;
const PAGE_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Counters {
    discovery: usize,
    pages: usize,
    lookups: usize,
    bytes: usize,
    reserved: usize,
    stop_reason: Option<&'static str>,
}

#[derive(Clone)]
pub(crate) struct DiagnosticSession {
    client: reqwest::Client,
    endpoint: String,
    counters: Arc<Mutex<Counters>>,
}

impl DiagnosticSession {
    pub(crate) fn new(client: reqwest::Client, root: &str) -> Self {
        Self {
            client,
            endpoint: format!(
                "{}/database/1/com.apple.photos.cloud/production/private",
                root.trim_end_matches('/')
            ),
            counters: Arc::default(),
        }
    }

    pub(crate) fn counts(&self) -> Value {
        let Ok(c) = self.counters.lock() else {
            return json!({"status":"unavailable"});
        };
        json!({"discovery_requests":c.discovery,"inventory_requests":c.pages,"lookup_requests":c.lookups,"completed_response_bytes":c.bytes,"reserved_response_allowance":c.reserved,"charged_response_allowance":c.bytes+c.reserved,"stop_reason":c.stop_reason.unwrap_or("none"),"failed_response_bytes":"not_established"})
    }

    async fn send(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, DiagnosticLookupError> {
        if headers != [("Content-type", "text/plain")] || body.len() > 64 * 1024 {
            return Err(DiagnosticLookupError::Unavailable);
        }
        let suffix = url
            .strip_prefix(&self.endpoint)
            .ok_or(DiagnosticLookupError::Unavailable)?;
        let path = suffix
            .split('?')
            .next()
            .ok_or(DiagnosticLookupError::Unavailable)?;
        let request: Value = serde_json::from_str(&body)
            .map_err(|_private_error| DiagnosticLookupError::Malformed)?;
        let limit = match path {
            "/records/query"
                if request.pointer("/query/recordType").and_then(Value::as_str)
                    == Some("CheckIndexingState")
                    && request.pointer("/zoneID/zoneName").and_then(Value::as_str)
                        == Some("PrimarySync") =>
            {
                1024 * 1024
            }
            "/zones/list" if request == json!({}) => 1024 * 1024,
            "/changes/zone"
                if request
                    .get("zones")
                    .and_then(Value::as_array)
                    .is_some_and(|zones| zones.len() == 1)
                    && request
                        .pointer("/zones/0/zoneID/zoneName")
                        .and_then(Value::as_str)
                        == Some("PrimarySync") =>
            {
                PAGE_LIMIT
            }
            "/records/lookup"
                if request
                    .get("records")
                    .and_then(Value::as_array)
                    .is_some_and(|records| records.len() == 1)
                    && request.pointer("/zoneID/zoneName").and_then(Value::as_str)
                        == Some("PrimarySync") =>
            {
                64 * 1024
            }
            _ => return Err(DiagnosticLookupError::Unavailable),
        };
        {
            let mut c = self
                .counters
                .lock()
                .map_err(|_private_error| DiagnosticLookupError::Unavailable)?;
            if limit
                > TRANSFER_LIMIT
                    .saturating_sub(c.bytes)
                    .saturating_sub(c.reserved)
            {
                c.stop_reason = Some("aggregate_byte_limit");
                return Err(DiagnosticLookupError::TooLarge);
            }
            let (count, cap) = match path {
                "/changes/zone" => (&mut c.pages, 1000),
                "/records/lookup" => (&mut c.lookups, 1),
                _ => (&mut c.discovery, 3),
            };
            if *count >= cap {
                c.stop_reason = Some("request_limit");
                return Err(DiagnosticLookupError::Unavailable);
            }
            *count += 1;
            c.reserved += limit;
        }
        // Keep failed/cancelled response reservations charged conservatively.
        let bytes = match diagnostic_bytes(&self.client, url, body, headers, limit).await {
            Ok(bytes) => bytes,
            Err(error) => {
                if let Ok(mut c) = self.counters.lock() {
                    c.stop_reason = Some(match error {
                        DiagnosticLookupError::TooLarge => "response_byte_limit",
                        DiagnosticLookupError::HttpStatus => "http_error",
                        DiagnosticLookupError::Malformed => "response_invalid",
                        DiagnosticLookupError::Request => "request_failed",
                        DiagnosticLookupError::Unavailable => "transport_unavailable",
                    });
                }
                return Err(error);
            }
        };
        {
            let mut c = self
                .counters
                .lock()
                .map_err(|_private_error| DiagnosticLookupError::Unavailable)?;
            c.reserved -= limit;
            c.bytes += bytes.len();
        }
        Ok(bytes)
    }
}

#[async_trait::async_trait]
impl PhotosSession for DiagnosticSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let bytes = self
            .send(url, body, headers)
            .await
            .map_err(|_private_error| anyhow::anyhow!("diagnostic_request_refused"))?;
        serde_json::from_slice(&bytes)
            .map_err(|_private_error| anyhow::anyhow!("diagnostic_response_invalid"))
    }
    async fn post_diagnostic(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<Value, DiagnosticLookupError> {
        let bytes = self.send(url, body, headers).await?;
        serde_json::from_slice(&bytes).map_err(|_private_error| DiagnosticLookupError::Malformed)
    }
    async fn post_changes_body(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        self.send(url, body, headers)
            .await
            .map_err(|_private_error| anyhow::anyhow!("diagnostic_request_refused"))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{DiagnosticSession, TRANSFER_LIMIT};
    use crate::icloud::photos::PhotosSession;
    use serde_json::json;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    fn session(root: &str) -> DiagnosticSession {
        DiagnosticSession::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .unwrap(),
            root,
        )
    }
    fn request() -> String {
        json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}]})
            .to_string()
    }
    #[tokio::test]
    async fn diagnostic_transport_limits_are_shared_reported_and_precede_dispatch() {
        let server = MockServer::start().await;
        let transport = session(&server.uri());
        let url = format!(
            "{}/database/1/com.apple.photos.cloud/production/private/changes/zone",
            server.uri()
        );
        transport.counters.lock().unwrap().pages = 1000;
        assert!(
            transport
                .clone_box()
                .post(&url, request(), &[("Content-type", "text/plain")])
                .await
                .is_err()
        );
        assert_eq!(transport.counts()["stop_reason"], "request_limit");
        {
            let mut c = transport.counters.lock().unwrap();
            c.pages = 0;
            c.bytes = TRANSFER_LIMIT - 1;
        }
        assert!(
            transport
                .post(&url, request(), &[("Content-type", "text/plain")])
                .await
                .is_err()
        );
        assert_eq!(transport.counts()["stop_reason"], "aggregate_byte_limit");
        assert_eq!(transport.counts()["inventory_requests"], 0);
        assert!(
            transport
                .post(&format!("{}/media", server.uri()), "{}".into(), &[])
                .await
                .is_err()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn diagnostic_transport_oversize_and_encoding_refusal_keep_charged_allowance_visible() {
        for compressed in [false, true] {
            let server = MockServer::start().await;
            let response = if compressed {
                ResponseTemplate::new(200)
                    .insert_header("Content-Encoding", "gzip")
                    .set_body_string("private-body")
            } else {
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; 1024 * 1024 + 1])
            };
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&server)
                .await;
            let transport = session(&server.uri());
            let url = format!(
                "{}/database/1/com.apple.photos.cloud/production/private/zones/list",
                server.uri()
            );
            assert!(
                transport
                    .post_changes_body(&url, "{}".into(), &[("Content-type", "text/plain")])
                    .await
                    .is_err()
            );
            let counts = transport.counts();
            assert_eq!(counts["completed_response_bytes"], 0);
            assert_eq!(counts["reserved_response_allowance"], 1024 * 1024);
            assert_eq!(
                counts["stop_reason"],
                if compressed {
                    "response_invalid"
                } else {
                    "response_byte_limit"
                }
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}
