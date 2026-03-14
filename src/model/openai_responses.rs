use crate::model::{ModelEngineError, ModelExchangeRequest, ModelExchangeResult};

#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct OpenAiResponsesEngine;

#[allow(dead_code)]
impl OpenAiResponsesEngine {
    pub async fn run(
        &self,
        _request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, ModelEngineError> {
        todo!("OpenAI Responses adapter is planned but not implemented yet");
    }
}
