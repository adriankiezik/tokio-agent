use futures::stream::BoxStream;
use serde::de::DeserializeOwned;
use tokio_agent_core::event::Event;
use tokio_agent_core::provider::ProviderError;
use tokio_util::sync::CancellationToken;

use crate::sse::{self, FrameAssembler};

pub(crate) async fn open_event_stream<A>(
    request: reqwest::RequestBuilder,
    assembler: A,
    cancel: CancellationToken,
) -> Result<BoxStream<'static, Event>, ProviderError>
where
    A: FrameAssembler,
{
    let response = tokio::select! {
        biased;
        () = cancel.cancelled() => return Ok(interrupted_stream(assembler)),
        response = request.send() => response,
    }
    .map_err(|error| ProviderError::retryable(format!("request failed: {error}")))?;

    let status = response.status();
    if !status.is_success() {
        let text = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(interrupted_stream(assembler)),
            text = response.text() => text.unwrap_or_default(),
        };
        let message = format!("HTTP {status}: {text}");
        return Err(if status.as_u16() == 429 || status.is_server_error() {
            ProviderError::retryable(message)
        } else {
            ProviderError::fatal(message)
        });
    }

    Ok(sse::spawn_event_stream(
        response.bytes_stream(),
        assembler,
        cancel,
    ))
}

pub(crate) async fn send_json<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    cancel: CancellationToken,
) -> Result<T, ProviderError> {
    let response = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(ProviderError::fatal("cancelled")),
        response = request.send() => response,
    }
    .map_err(|error| ProviderError::retryable(format!("request failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let message = format!("HTTP {status}: {text}");
        return Err(if status.as_u16() == 429 || status.is_server_error() {
            ProviderError::retryable(message)
        } else {
            ProviderError::fatal(message)
        });
    }
    response
        .json()
        .await
        .map_err(|error| ProviderError::fatal(format!("invalid JSON response: {error}")))
}

pub(crate) fn interrupted_stream<A>(assembler: A) -> BoxStream<'static, Event>
where
    A: FrameAssembler,
{
    Box::pin(futures::stream::once(
        async move { assembler.interrupted() },
    ))
}
