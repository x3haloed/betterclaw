mod accumulator;
mod copilot_session;
mod events;
mod openai_chatcompletions;
mod openai_compat;
mod openai_responses;
mod qwen_agent;
mod reasoning;
mod schema_strict;
mod stub;
mod trace;
mod transport;
mod types;

pub use accumulator::ExchangeAccumulator;
pub use events::ModelEvent;
pub use openai_chatcompletions::OpenAiChatCompletionsEngine;
pub use openai_compat::OpenAiCompatibleConfig;
pub use openai_responses::OpenAiResponsesEngine;
pub use reasoning::{split_inline_reasoning, strip_reasoning_tags};
pub use schema_strict::{normalize_schema_strict, validate_strict_schema};
pub use stub::StubModelEngine;
pub use trace::ModelTrace;
pub use trace::{RawFrame, RawModelTrace, TraceBlob, TraceDetail, TraceOutcome};
pub use transport::{AccumulationMode, ReasoningMode, TransportKind};
pub use types::{
    ModelEngine, ModelEngineError, ModelExchangeRequest, ModelExchangeResult, ModelMessage,
    ModelRunner, ModelToolCallMessage, ModelToolFunctionMessage, ModelUsage, ReducedToolCall,
};
