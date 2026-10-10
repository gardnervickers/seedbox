//! Local transport only. The executable decides whether to start a server;
//! connection failures never launch processes or silently change endpoints.
#![cfg(unix)]
mod sse;
pub use sse::LogStream;
pub use sse::{EventStream, FrameStream, SimulationStream};

use reqwest::{Method, RequestBuilder};
use seedbox_api::{ApiError, ServerInfo, Simulation, SimulationId, SimulationList, SubmitRequest};
use serde::de::DeserializeOwned;
use std::{path::Path, time::Duration};

const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("server stream: {0}")]
    Stream(String),
    #[error("request randomness unavailable: {0}")]
    Random(String),
    #[error("server connection: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("server response: {0}")]
    Protocol(#[from] serde_json::Error),
    #[error("server response exceeds {RESPONSE_LIMIT} bytes")]
    ResponseTooLarge,
    #[error("server rejected request ({code:?}): {message}")]
    Rejected {
        code: seedbox_api::ErrorCode,
        message: String,
    },
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn request_id() -> Result<String, Error> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|e| Error::Random(e.to_string()))?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
    pub fn connect_to(socket: &Path) -> Result<Self, Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .unix_socket(socket.to_owned())
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .build()?,
        })
    }

    pub async fn info(&self) -> Result<ServerInfo, Error> {
        self.document(self.request(Method::GET, "/v1")).await
    }

    pub async fn submit(&self, request: &SubmitRequest) -> Result<Simulation, Error> {
        self.document(self.request(Method::POST, "/v1/simulations").json(request))
            .await
    }

    pub async fn submit_seeds(
        &self,
        request: &seedbox_api::SeedRangeRequest,
    ) -> Result<seedbox_api::SeedRangeStatus, Error> {
        self.document(self.request(Method::POST, "/v1/seed-ranges").json(request))
            .await
    }
    pub async fn seed_range(
        &self,
        id: &SimulationId,
    ) -> Result<seedbox_api::SeedRangeStatus, Error> {
        self.document(self.request(Method::GET, &format!("/v1/simulations/{id}/seeds")))
            .await
    }
    pub async fn stop_seeds(
        &self,
        id: &SimulationId,
    ) -> Result<seedbox_api::SeedRangeStatus, Error> {
        self.document(self.request(Method::POST, &format!("/v1/simulations/{id}/seeds/stop")))
            .await
    }

    pub async fn compare(
        &self,
        baseline: &SimulationId,
        simulation: &SimulationId,
    ) -> Result<seedbox_api::SimulationComparison, Error> {
        self.document(self.request(
            Method::GET,
            &format!("/v1/simulations/{baseline}/compare/{simulation}"),
        ))
        .await
    }

    pub async fn describe(&self, id: &SimulationId) -> Result<Simulation, Error> {
        self.document(self.request(Method::GET, &format!("/v1/simulations/{id}")))
            .await
    }

    pub async fn checkpoints(
        &self,
        id: &SimulationId,
    ) -> Result<Vec<seedbox_api::ControlReceipt>, Error> {
        self.document(self.request(Method::GET, &format!("/v1/simulations/{id}/checkpoints")))
            .await
    }

    pub async fn checkpoint(
        &self,
        id: &SimulationId,
        name: &str,
    ) -> Result<seedbox_api::ControlReceipt, Error> {
        let mut url = reqwest::Url::parse("http://seedbox.local/v1/simulations/")
            .map_err(|e| Error::Stream(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| Error::Stream("invalid checkpoint URL".into()))?
            .pop_if_empty()
            .push(&id.to_string())
            .push("checkpoints")
            .push(name);
        self.document(self.http.get(url)).await
    }

    pub async fn delete(&self, id: &SimulationId) -> Result<serde_json::Value, Error> {
        self.document(self.request(Method::DELETE, &format!("/v1/simulations/{id}")))
            .await
    }

    pub async fn delete_checkpoint(
        &self,
        id: &SimulationId,
        name: &str,
    ) -> Result<serde_json::Value, Error> {
        let mut url = reqwest::Url::parse("http://seedbox.local/v1/simulations/")
            .map_err(|e| Error::Stream(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| Error::Stream("invalid checkpoint URL".into()))?
            .pop_if_empty()
            .push(id.as_str())
            .push("checkpoints")
            .push(name);
        self.document(self.http.delete(url)).await
    }

    pub async fn stop(&self, id: &SimulationId) -> Result<Simulation, Error> {
        self.document(self.request(Method::POST, &format!("/v1/simulations/{id}/stop")))
            .await
    }

    pub async fn watch(&self, id: &SimulationId) -> Result<SimulationStream, Error> {
        Ok(sse::simulations(
            self.event_response(&format!("/v1/simulations/{id}/watch"))
                .await?,
        ))
    }

    pub async fn request_control(
        &self,
        id: &SimulationId,
        request: &seedbox_api::ControlRequest,
    ) -> Result<seedbox_api::ControlReceipt, Error> {
        self.document(
            self.request(Method::POST, &format!("/v1/simulations/{id}/controls"))
                .json(request),
        )
        .await
    }

    pub async fn control_receipt(
        &self,
        id: &SimulationId,
        request_id: &str,
    ) -> Result<seedbox_api::ControlReceipt, Error> {
        let mut url = reqwest::Url::parse("http://seedbox.local/v1/simulations/")
            .map_err(|e| Error::Stream(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| Error::Stream("invalid API base URL".into()))?
            .pop_if_empty()
            .push(&id.to_string())
            .push("controls")
            .push(request_id);
        self.document(self.request(Method::GET, url.path())).await
    }

    pub async fn playback(
        &self,
        id: &SimulationId,
    ) -> Result<seedbox_api::PlaybackObservation, Error> {
        self.document(self.request(Method::GET, &format!("/v1/simulations/{id}/playback")))
            .await
    }

    pub async fn events(&self, id: &SimulationId) -> Result<EventStream, Error> {
        Ok(sse::events(
            self.event_response(&format!("/v1/simulations/{id}/events"))
                .await?,
        ))
    }

    /// The world's machines, links and faults as the server has observed them.
    pub async fn world(&self, id: &SimulationId) -> Result<seedbox_api::WorldState, Error> {
        self.document(self.request(Method::GET, &format!("/v1/simulations/{id}/world")))
            .await
    }

    /// Routed frames from now on. Batches lost by a slow reader are reported as gaps.
    pub async fn frames(&self, id: &SimulationId) -> Result<FrameStream, Error> {
        Ok(sse::frames(
            self.event_response(&format!("/v1/simulations/{id}/frames"))
                .await?,
        ))
    }

    pub async fn all_logs(&self, id: &SimulationId) -> Result<LogStream, Error> {
        Ok(sse::logs(
            self.event_response(&format!("/v1/simulations/{id}/logs"))
                .await?,
        ))
    }

    pub async fn logs(&self, id: &SimulationId, machine: &str) -> Result<LogStream, Error> {
        // URL-encode the machine as one path segment, never as API syntax.
        let mut url = reqwest::Url::parse("http://seedbox.local/v1/simulations/")
            .map_err(|e| Error::Stream(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| Error::Stream("invalid API base URL".into()))?
            .pop_if_empty()
            .push(&id.to_string())
            .push("logs")
            .push(machine);
        Ok(sse::logs(self.event_response(url.path()).await?))
    }

    async fn event_response(&self, path: &str) -> Result<reqwest::Response, Error> {
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            self.request(Method::GET, path).send(),
        )
        .await
        .map_err(|_| Error::Stream("timed out waiting for watch headers".into()))??;
        if !response.status().is_success() {
            let _: Simulation = self.decode(response).await?;
            return Err(Error::Stream("unexpected successful error response".into()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.split(';').next() != Some("text/event-stream") {
            return Err(Error::Stream("expected text/event-stream".into()));
        }
        Ok(response)
    }

    pub async fn list(&self, after: Option<&SimulationId>) -> Result<SimulationList, Error> {
        let mut request = self.request(Method::GET, "/v1/simulations");
        if let Some(id) = after {
            request = request.query(&[("after", id.as_str())]);
        }
        self.document(request).await
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        // The authority is syntactic only; every connection uses the explicit
        // Unix socket, with ambient proxies and redirects disabled.
        self.http
            .request(method, format!("http://seedbox.local{path}"))
    }

    async fn document<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Error> {
        let response = request.timeout(Duration::from_secs(30)).send().await?;
        self.decode(response).await
    }

    async fn decode<T: DeserializeOwned>(
        &self,
        mut response: reqwest::Response,
    ) -> Result<T, Error> {
        let success = response.status().is_success();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if chunk.len() > RESPONSE_LIMIT - bytes.len() {
                return Err(Error::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        if !success {
            let error: ApiError = serde_json::from_slice(&bytes)?;
            return Err(Error::Rejected {
                code: error.code,
                message: error.message,
            });
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}
