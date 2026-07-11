use std::future::Future;
use std::pin::Pin;

use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::event::Event;
use crate::message::Message;
use crate::tool::ToolDef;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    pub tools: bool,
    pub streaming: bool,
    pub caching: bool,
    pub vision: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("provider error ({}): {message}", if *.retryable { "retryable" } else { "fatal" })]
pub struct ProviderError {
    pub retryable: bool,
    pub message: String,
}

impl ProviderError {
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            retryable: false,
            message: message.into(),
        }
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            message: message.into(),
        }
    }
}

pub trait Provider: Send + Sync {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>>;

    fn capabilities(&self) -> Capabilities;

    fn count_tokens<'a>(
        &'a self,
        _req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<u64, ProviderError>> {
        Box::pin(async { Err(ProviderError::fatal("token counting is not supported")) })
    }

    fn supports_native_compaction(&self) -> bool {
        false
    }

    fn compact<'a>(
        &'a self,
        _req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Option<Vec<Message>>, ProviderError>> {
        Box::pin(async { Ok(None) })
    }
}
