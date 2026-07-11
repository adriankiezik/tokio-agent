use tokio_agent_core::Event;
use tokio_agent_core::message::Message;

use crate::anthropic::Assembler;
pub use crate::sse::SseFrame;

#[must_use]
pub fn parse_sse_fixture(fixture: &str) -> Vec<SseFrame> {
    crate::sse::decode_fixture(fixture)
}

pub fn run_fixture(
    frames: &[SseFrame],
    parse_frame: impl FnMut(&SseFrame) -> Vec<Event>,
) -> Vec<Event> {
    frames.iter().flat_map(parse_frame).collect()
}

#[must_use]
pub fn assemble_anthropic(frames: &[SseFrame]) -> Vec<Event> {
    let mut assembler = Assembler::new();
    run_fixture(frames, |frame| assembler.push(&frame.data))
}

#[must_use]
pub fn assemble_openai(frames: &[SseFrame]) -> Vec<Event> {
    let mut assembler = crate::openai::Assembler::new();
    run_fixture(frames, |frame| assembler.push(&frame.data))
}

#[must_use]
pub fn assembled_message(events: &[Event]) -> &Message {
    match events.last() {
        Some(Event::Done { message, .. }) => message,
        other => panic!("stream must terminate in Event::Done, got {other:?}"),
    }
}
