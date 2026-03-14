mod accumulator;
mod copilot_session;
mod events;
mod openai_chatcompletions;
mod openai_responses;
mod qwen_agent;
mod stub;
mod trace;
mod transport;
mod types;

pub use accumulator::ExchangeAccumulator;
pub use events::ModelEvent;
pub use openai_chatcompletions::{OpenAiChatCompletionsConfig, OpenAiChatCompletionsEngine};
pub use stub::StubModelEngine;
pub use trace::{RawFrame, RawModelTrace, TraceBlob, TraceDetail, TraceOutcome};
pub use transport::TransportKind;
pub use types::{
    ModelEngine, ModelEngineError, ModelExchangeRequest, ModelExchangeResult, ModelMessage,
    ModelRunner, ModelToolCallMessage, ModelToolFunctionMessage, ModelUsage, ReducedToolCall,
};
pub use trace::ModelTrace;
