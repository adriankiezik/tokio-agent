pub mod anthropic;
pub mod conformance;
pub mod openai;
mod sse;
mod transport;

pub use anthropic::Anthropic;
pub use openai::OpenAi;

use futures::stream::BoxStream;
use tokio_agent_core::event::Event;
use tokio_agent_core::provider::{BoxFuture, Capabilities, Provider, ProviderError, Request};
use tokio_util::sync::CancellationToken;

pub enum AnyProvider {
    Anthropic(Anthropic),
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
            AnyProvider::OpenAi(p) => p.stream(req, cancel),
        }
    }

    fn capabilities(&self) -> Capabilities {
        match self {
            AnyProvider::Anthropic(p) => p.capabilities(),
            AnyProvider::OpenAi(p) => p.capabilities(),
        }
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<u64, ProviderError>> {
        match self {
            AnyProvider::Anthropic(p) => p.count_tokens(req, cancel),
            AnyProvider::OpenAi(p) => p.count_tokens(req, cancel),
        }
    }
}
