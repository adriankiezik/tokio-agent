pub mod anthropic;
pub mod conformance;
pub mod deepseek;
pub mod openai;
mod sse;
mod transport;

pub use anthropic::Anthropic;
pub use deepseek::DeepSeek;
pub use openai::OpenAi;

use futures::stream::BoxStream;
use tokio_agent_core::event::Event;
use tokio_agent_core::provider::{BoxFuture, Capabilities, Provider, ProviderError, Request};
use tokio_util::sync::CancellationToken;

pub enum AnyProvider {
    Anthropic(Anthropic),
    DeepSeek(DeepSeek),
    OpenAi(OpenAi),
}

impl Provider for AnyProvider {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        match self {
            AnyProvider::Anthropic(p) => p.stream(req, cancel),
            AnyProvider::DeepSeek(p) => p.stream(req, cancel),
            AnyProvider::OpenAi(p) => p.stream(req, cancel),
        }
    }

    fn capabilities(&self) -> Capabilities {
        match self {
            AnyProvider::Anthropic(p) => p.capabilities(),
            AnyProvider::DeepSeek(p) => p.capabilities(),
            AnyProvider::OpenAi(p) => p.capabilities(),
        }
    }

    fn supports_native_compaction(&self) -> bool {
        match self {
            AnyProvider::Anthropic(p) => p.supports_native_compaction(),
            AnyProvider::DeepSeek(p) => p.supports_native_compaction(),
            AnyProvider::OpenAi(p) => p.supports_native_compaction(),
        }
    }

    fn compact<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Option<Vec<tokio_agent_core::message::Message>>, ProviderError>> {
        match self {
            AnyProvider::Anthropic(p) => p.compact(req, cancel),
            AnyProvider::DeepSeek(p) => p.compact(req, cancel),
            AnyProvider::OpenAi(p) => p.compact(req, cancel),
        }
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<u64, ProviderError>> {
        match self {
            AnyProvider::Anthropic(p) => p.count_tokens(req, cancel),
            AnyProvider::DeepSeek(p) => p.count_tokens(req, cancel),
            AnyProvider::OpenAi(p) => p.count_tokens(req, cancel),
        }
    }
}
