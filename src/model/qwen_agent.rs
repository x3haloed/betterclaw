use crate::model::{ModelEngineError, ModelExchangeRequest, ModelExchangeResult};

#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct QwenAgentEngine;

#[allow(dead_code)]
impl QwenAgentEngine {
    pub async fn run(
        &self,
        _request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, ModelEngineError> {
        todo!("Qwen-Agent adapter is planned but not implemented yet");
    }
}
