use futures::Stream;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio_agent_core::event::Event;
use tokio_agent_core::provider::ProviderError;
use tokio_util::sync::CancellationToken;

pub trait FrameAssembler: Send + 'static {
    fn push(&mut self, data: &str) -> Vec<Event>;
    fn interrupted(&self) -> Event;
    fn failed(&self, error: ProviderError) -> Event;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

pub fn spawn_event_stream<S, B, E, A>(
    body: S,
    mut assembler: A,
    cancel: CancellationToken,
) -> BoxStream<'static, Event>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
    A: FrameAssembler,
{
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        futures::pin_mut!(body);
        let mut decoder = SseDecoder::new();
        let mut terminated = false;

        loop {
            let chunk = tokio::select! {
                biased;
                () = tx.closed() => return,
                () = cancel.cancelled() => {
                    let _ = tx.send(assembler.interrupted());
                    return;
                }
                chunk = body.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    let error =
                        ProviderError::retryable(format!("response stream failed: {error}"));
                    let _ = tx.send(assembler.failed(error));
                    return;
                }
            };
            for frame in decoder.push(bytes.as_ref()) {
                for event in assembler.push(&frame.data) {
                    let is_done = matches!(event, Event::Done { .. });
                    if let Event::Unknown(frame) = &event {
                        tracing::debug!(provider = %frame.provider, payload = %frame.payload, "unknown provider frame");
                    }
                    if tx.send(event).is_err() {
                        return;
                    }
                    terminated |= is_done;
                }
                if terminated {
                    return;
                }
            }
        }

        for frame in decoder.finish() {
            for event in assembler.push(&frame.data) {
                terminated |= matches!(event, Event::Done { .. });
                if let Event::Unknown(frame) = &event {
                    tracing::debug!(provider = %frame.provider, payload = %frame.payload, "unknown provider frame");
                }
                if tx.send(event).is_err() {
                    return;
                }
            }
        }

        if !terminated {
            let error =
                ProviderError::retryable("response stream ended without a terminal provider event");
            let _ = tx.send(assembler.failed(error));
        }
    });
    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }))
}

struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = find_double_newline(&self.buf) {
            let block: Vec<u8> = self.buf.drain(..pos + 2).collect();
            if let Ok(text) = std::str::from_utf8(&block)
                && let Some(frame) = parse_block(text)
            {
                out.push(frame);
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<SseFrame> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let block = std::mem::take(&mut self.buf);
        match std::str::from_utf8(&block).ok().and_then(parse_block) {
            Some(frame) => vec![frame],
            None => Vec::new(),
        }
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

fn parse_block(block: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim_start().to_owned());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(SseFrame {
            event,
            data: data.join("\n"),
        })
    }
}

pub(crate) fn decode_fixture(fixture: &str) -> Vec<SseFrame> {
    let mut decoder = SseDecoder::new();
    let mut frames = decoder.push(fixture.as_bytes());
    frames.extend(decoder.finish());
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, stream};
    use std::pin::Pin;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::task::{Context, Poll};
    use tokio_agent_core::event::StopReason;
    use tokio_agent_core::message::{ContentBlock, Message, ProviderMetadata, Role};

    struct TextAssembler(String);

    impl FrameAssembler for TextAssembler {
        fn push(&mut self, data: &str) -> Vec<Event> {
            if data == "done" {
                return vec![Event::Done {
                    stop: StopReason::EndTurn,
                    message: Message {
                        role: Role::Assistant,
                        blocks: vec![],
                        usage: None,
                    },
                }];
            }
            self.0.push_str(data);
            vec![Event::TextDelta {
                text: data.to_owned(),
            }]
        }

        fn interrupted(&self) -> Event {
            Event::Done {
                stop: StopReason::Interrupted,
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Text {
                        text: self.0.clone(),
                        meta: ProviderMetadata::default(),
                    }],
                    usage: None,
                },
            }
        }

        fn failed(&self, error: ProviderError) -> Event {
            let message = if self.0.is_empty() {
                Message {
                    role: Role::Assistant,
                    blocks: vec![],
                    usage: None,
                }
            } else {
                let Event::Done { message, .. } = self.interrupted() else {
                    unreachable!()
                };
                message
            };
            Event::Failed {
                retryable: error.retryable,
                error: error.message,
                message,
            }
        }
    }

    struct DropObserved<S> {
        inner: Pin<Box<S>>,
        dropped: Arc<AtomicBool>,
    }

    impl<S> Stream for DropObserved<S>
    where
        S: Stream,
    {
        type Item = S::Item;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.inner.as_mut().poll_next(cx)
        }
    }

    impl<S> Drop for DropObserved<S> {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn cancellation_emits_interrupted_done_with_partial_message() {
        let body = stream::once(async { Ok::<_, reqwest::Error>(b"data: partial\n\n".to_vec()) })
            .chain(stream::pending());
        let cancel = CancellationToken::new();
        let mut events = spawn_event_stream(body, TextAssembler(String::new()), cancel.clone());

        assert!(
            matches!(events.next().await, Some(Event::TextDelta { text }) if text == "partial")
        );
        cancel.cancel();

        let Some(Event::Done { stop, message }) = events.next().await else {
            panic!("cancellation must terminate with Done");
        };
        assert_eq!(stop, StopReason::Interrupted);
        assert!(matches!(&message.blocks[0], ContentBlock::Text { text, .. } if text == "partial"));
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn provider_done_is_terminal_and_emitted_exactly_once() {
        let body = stream::iter([Ok::<_, reqwest::Error>(
            b"data: done\n\ndata: ignored\n\n".to_vec(),
        )])
        .chain(stream::pending());
        let mut events =
            spawn_event_stream(body, TextAssembler(String::new()), CancellationToken::new());

        assert!(matches!(
            events.next().await,
            Some(Event::Done {
                stop: StopReason::EndTurn,
                ..
            })
        ));
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_event_stream_drops_pending_response_body() {
        let dropped = Arc::new(AtomicBool::new(false));
        let body = DropObserved {
            inner: Box::pin(stream::pending::<Result<Vec<u8>, reqwest::Error>>()),
            dropped: dropped.clone(),
        };
        let events =
            spawn_event_stream(body, TextAssembler(String::new()), CancellationToken::new());
        tokio::task::yield_now().await;
        drop(events);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the consumer must stop the SSE task");
    }

    #[tokio::test]
    async fn eof_is_failure_before_and_after_partial_content() {
        let mut empty = spawn_event_stream(
            stream::empty::<Result<Vec<u8>, std::io::Error>>(),
            TextAssembler(String::new()),
            CancellationToken::new(),
        );
        let Some(Event::Failed {
            retryable, message, ..
        }) = empty.next().await
        else {
            panic!("EOF must be a transport failure");
        };
        assert!(retryable);
        assert!(message.blocks.is_empty());
        assert!(empty.next().await.is_none());

        let body = stream::once(async { Ok::<_, std::io::Error>(b"data: partial\n\n".to_vec()) });
        let mut partial =
            spawn_event_stream(body, TextAssembler(String::new()), CancellationToken::new());
        assert!(matches!(
            partial.next().await,
            Some(Event::TextDelta { .. })
        ));
        let Some(Event::Failed {
            retryable, message, ..
        }) = partial.next().await
        else {
            panic!("EOF after content must preserve a partial message");
        };
        assert!(retryable);
        assert!(matches!(&message.blocks[0], ContentBlock::Text { text, .. } if text == "partial"));
        assert!(partial.next().await.is_none());
    }

    #[tokio::test]
    async fn read_error_is_failure_before_and_after_partial_content() {
        let failure = || std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let mut empty = spawn_event_stream(
            stream::once(async {
                Err::<Vec<u8>, _>(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ))
            }),
            TextAssembler(String::new()),
            CancellationToken::new(),
        );
        let Some(Event::Failed {
            retryable,
            error,
            message,
        }) = empty.next().await
        else {
            panic!("read error must be a transport failure");
        };
        assert!(retryable);
        assert!(error.contains("reset"));
        assert!(message.blocks.is_empty());

        let body = stream::iter([Ok(b"data: partial\n\n".to_vec()), Err(failure())]);
        let mut partial =
            spawn_event_stream(body, TextAssembler(String::new()), CancellationToken::new());
        assert!(matches!(
            partial.next().await,
            Some(Event::TextDelta { .. })
        ));
        let Some(Event::Failed {
            retryable, message, ..
        }) = partial.next().await
        else {
            panic!("read error after content must preserve a partial message");
        };
        assert!(retryable);
        assert!(matches!(&message.blocks[0], ContentBlock::Text { text, .. } if text == "partial"));
        assert!(partial.next().await.is_none());
    }
}
