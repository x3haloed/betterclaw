use crate::model::{ModelEngineError, ModelExchangeRequest, ModelExchangeResult};

#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct CopilotSessionEngine;

#[allow(dead_code)]
impl CopilotSessionEngine {
    pub async fn run(
        &self,
        _request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, ModelEngineError> {
        todo!("Copilot session adapter is planned but not implemented yet");
    }
}
