// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{sync::Arc, time::Duration};

use backoff::ExponentialBackoff;
use reqwest::{Client, StatusCode};
use sui_storage::blob::Blob;
use sui_types::full_checkpoint_content::CheckpointData;
use tracing::debug;
use url::Url;

use crate::ingestion::error::{Error, Result};
use crate::metrics::IndexerMetrics;

/// Wait at most this long between retries for transient errors.
const MAX_TRANSIENT_RETRY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) struct IngestionClient {
    url: Url,
    client: Client,
    /// Wrap the metrics in an `Arc` to keep copies of the client cheap.
    metrics: Arc<IndexerMetrics>,
}

impl IngestionClient {
    pub(crate) fn new(url: Url, metrics: Arc<IndexerMetrics>) -> Result<Self> {
        Ok(Self {
            url,
            client: Client::builder().build()?,
            metrics,
        })
    }

    /// Fetch a checkpoint from the remote store. Repeatedly retries transient errors with an
    /// exponential backoff (up to [MAX_RETRY_INTERVAL]), but will immediately return
    /// non-transient errors, which include all client errors, except timeouts and rate limiting.
    pub(crate) async fn fetch(&self, checkpoint: u64) -> Result<Arc<CheckpointData>> {
        // SAFETY: The path being joined is statically known to be valid.
        let url = self
            .url
            .join(&format!("/{checkpoint}.chk"))
            .expect("Unexpected invalid URL");

        let request = move || {
            let url = url.clone();
            async move {
                let response = self
                    .client
                    .get(url)
                    .send()
                    .await
                    .expect("Unexpected error building request");

                use backoff::Error as BE;
                match response.status() {
                    code if code.is_success() => Ok(response),

                    // Treat 404s as a special case so we can match on this error type.
                    code @ StatusCode::NOT_FOUND => {
                        debug!(checkpoint, %code, "Checkpoint not found");
                        Err(BE::permanent(Error::NotFound(checkpoint)))
                    }

                    // Timeouts are a client error but they are usually transient.
                    code @ StatusCode::REQUEST_TIMEOUT => {
                        debug!(checkpoint, %code, "Transient error, retrying...");
                        self.metrics.total_ingested_transient_retries.inc();
                        Err(BE::transient(Error::HttpError(checkpoint, code)))
                    }

                    // Rate limiting is also a client error, but the backoff will eventually widen the
                    // interval appropriately.
                    code @ StatusCode::TOO_MANY_REQUESTS => {
                        debug!(checkpoint, %code, "Transient error, retrying...");
                        self.metrics.total_ingested_transient_retries.inc();
                        Err(BE::transient(Error::HttpError(checkpoint, code)))
                    }

                    // Assume that if the server is facing difficulties, it will recover eventually.
                    code if code.is_server_error() => {
                        debug!(checkpoint, %code, "Transient error, retrying...");
                        self.metrics.total_ingested_transient_retries.inc();
                        Err(BE::transient(Error::HttpError(checkpoint, code)))
                    }

                    // For everything else, assume it's a permanent error and don't retry.
                    code => {
                        debug!(checkpoint, %code, "Permanent error, giving up!");
                        Err(BE::permanent(Error::HttpError(checkpoint, code)))
                    }
                }
            }
        };

        // Keep backing off until we are waiting for the max interval, but don't give up.
        let backoff = ExponentialBackoff {
            max_interval: MAX_TRANSIENT_RETRY_INTERVAL,
            max_elapsed_time: None,
            ..Default::default()
        };

        let guard = self.metrics.ingested_checkpoint_latency.start_timer();

        let bytes = backoff::future::retry(backoff, request)
            .await?
            .bytes()
            .await?;

        let data: CheckpointData =
            Blob::from_bytes(&bytes).map_err(|e| Error::DeserializationError(checkpoint, e))?;

        let elapsed = guard.stop_and_record();
        debug!(
            checkpoint,
            "Fetched checkpoint in {:.03}ms",
            elapsed * 1000.0
        );

        self.metrics.total_ingested_checkpoints.inc();
        self.metrics.total_ingested_bytes.inc_by(bytes.len() as u64);

        self.metrics
            .total_ingested_transactions
            .inc_by(data.transactions.len() as u64);

        self.metrics.total_ingested_events.inc_by(
            data.transactions
                .iter()
                .map(|tx| tx.events.as_ref().map_or(0, |evs| evs.data.len()) as u64)
                .sum(),
        );

        self.metrics.total_ingested_inputs.inc_by(
            data.transactions
                .iter()
                .map(|tx| tx.input_objects.len() as u64)
                .sum(),
        );

        self.metrics.total_ingested_outputs.inc_by(
            data.transactions
                .iter()
                .map(|tx| tx.output_objects.len() as u64)
                .sum(),
        );

        Ok(Arc::new(data))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use wiremock::{
        matchers::{method, path_regex},
        Mock, MockServer, Request, Respond, ResponseTemplate,
    };

    use crate::metrics::tests::test_metrics;

    use super::*;

    async fn respond_with(server: &MockServer, response: impl Respond + 'static) {
        Mock::given(method("GET"))
            .and(path_regex(r"/\d+.chk"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn status(code: StatusCode) -> ResponseTemplate {
        ResponseTemplate::new(code.as_u16())
    }

    fn test_client(uri: String) -> IngestionClient {
        IngestionClient::new(Url::parse(&uri).unwrap(), Arc::new(test_metrics())).unwrap()
    }

    #[tokio::test]
    async fn not_found() {
        let server = MockServer::start().await;
        respond_with(&server, status(StatusCode::NOT_FOUND)).await;

        let client = test_client(server.uri());
        let error = client.fetch(42).await.unwrap_err();

        assert!(matches!(error, Error::NotFound(42)));
    }

    #[tokio::test]
    async fn client_error() {
        let server = MockServer::start().await;
        respond_with(&server, status(StatusCode::IM_A_TEAPOT)).await;

        let client = test_client(server.uri());
        let error = client.fetch(42).await.unwrap_err();

        assert!(matches!(
            error,
            Error::HttpError(42, StatusCode::IM_A_TEAPOT)
        ));
    }

    #[tokio::test]
    async fn transient_server_error() {
        let server = MockServer::start().await;

        let times: Mutex<u64> = Mutex::new(0);
        respond_with(&server, move |_: &Request| {
            let mut times = times.lock().unwrap();
            *times += 1;
            status(match *times {
                1 => StatusCode::INTERNAL_SERVER_ERROR,
                2 => StatusCode::REQUEST_TIMEOUT,
                3 => StatusCode::TOO_MANY_REQUESTS,
                _ => StatusCode::IM_A_TEAPOT,
            })
        })
        .await;

        let client = test_client(server.uri());
        let error = client.fetch(42).await.unwrap_err();

        assert!(matches!(
            error,
            Error::HttpError(42, StatusCode::IM_A_TEAPOT)
        ));
    }
}
