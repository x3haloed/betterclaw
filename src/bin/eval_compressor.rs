use anyhow::{Context, Result};
use betterclaw::model::{
    OpenAiChatCompletionsEngine, OpenAiCompatibleConfig, OpenAiResponsesEngine, MessageContent,
    ModelEngine, ModelExchangeRequest, ModelMessage, ModelRunner,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::time::SystemTime;

#[derive(Debug, Deserialize)]
struct EvalConfig {
    compressor: ModelProviderConfig,
    evaluator: ModelProviderConfig,
    candidate_prompt: String,
    rubric: String,
    evaluator_prompt: Option<String>,
    scenarios: Vec<TestScenario>,
}

#[derive(Debug, Deserialize)]
struct ModelProviderConfig {
    provider: String,
    model: String,
    base_url: Option<String>,
    api_key_env: Option<String>,
    mode: Option<String>,
    hyperparams: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct TestScenario {
    name: String,
    prior_wake_pack: Option<Value>,
    active_invariants: Vec<Value>,
    thread_history: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct EvalReport {
    timestamp: u64,
    compressor_model: String,
    evaluator_model: String,
    candidate_prompt: String,
    scenario_reports: Vec<ScenarioReport>,
    average_score: f64,
}

#[derive(Debug, Serialize)]
struct ScenarioReport {
    scenario_name: String,
    compressor_raw_response: Option<String>,
    score: f64,
    reasoning: String,
}

#[derive(Debug, Deserialize)]
struct EvaluatorOutput {
    score: f64,
    reasoning: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct IntermediateScenario {
    scenario: TestScenario,
    compressor_output: String,
}

fn build_engine(config: &ModelProviderConfig) -> Result<ModelEngine> {
    let mut api_key = None;
    if let Some(env_var) = &config.api_key_env {
        api_key = env::var(env_var).ok();
    }

    let compat_config = OpenAiCompatibleConfig {
        base_url: config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        provider_name: config.provider.clone(),
        bearer_token: api_key,
        ..OpenAiCompatibleConfig::default()
    };

    if config.provider == "stub" {
        return Ok(ModelEngine::stub(betterclaw::model::StubModelEngine::default()));
    }

    let mode = config.mode.as_deref().unwrap_or("chat");
    match mode {
        "responses" => Ok(ModelEngine::openai_responses(OpenAiResponsesEngine::new(
            compat_config,
        )?)),
        _ => Ok(ModelEngine::openai_chat_completions(
            OpenAiChatCompletionsEngine::new(compat_config)?,
        )),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        anyhow::bail!("Usage: {} <path_to_config.json> [--mode compressor|evaluator]", args[0]);
    }

    let config_path = &args[1];
    let mode = if args.len() > 2 && args[2] == "--mode" {
        args.get(3).map(|s| s.as_str()).unwrap_or("both")
    } else {
        "both"
    };

    let config_str = fs::read_to_string(config_path).context("Failed to read config file")?;
    let config: EvalConfig =
        serde_json::from_str(&config_str).context("Failed to parse config file")?;

    if mode == "compressor" || mode == "both" {
        let compressor_engine = build_engine(&config.compressor)?;
        let mut intermediate_results = Vec::new();

        for scenario in &config.scenarios {
            println!("Running compressor for scenario: {}", scenario.name);

            let user_content = json!({
                "task": "distill_thread_frontier",
                "namespace_id": "eval",
                "thread_id": "eval_thread",
                "frontier_turn_id": "eval_turn",
                "previous_wake_pack": scenario.prior_wake_pack,
                "active_invariants": scenario.active_invariants,
                "thread_history_up_to_frontier": scenario.thread_history,
                "output_contract": {
                    "wake_pack": "string",
                    "facts": [{
                        "fact_id":"string",
                        "text":"string",
                        "citations":["entry_id"],
                        "support_excerpt":"string",
                        "falsifier":"string"
                    }],
                    "invariant_adds": [{
                        "text":"string",
                        "citations":["entry_id"],
                        "support_excerpt":"string",
                        "falsifier":"string",
                        "why_it_holds":"string",
                        "supersedes_ids":["artifact_id"],
                        "derived_from_fact_ids":["fact_id"]
                    }],
                    "invariant_removes": ["artifact_id"],
                    "policies": ["string"],
                    "preferences": ["string"],
                    "hypotheses": ["string"],
                    "drift_flags": [{"text":"string","citations":["entry_id"]}],
                    "drift_contradictions": [{"text":"string","citations":["entry_id"]}],
                    "drift_merges": [{"text":"string","citations":["entry_id"]}],
                    "summary": "optional string"
                }
            });

            let compressor_request = ModelExchangeRequest { role: Some("compressor".to_string()),
                model: config.compressor.model.clone(),
                messages: vec![
                    ModelMessage {
                        role: "system".to_string(),
                        content: Some(MessageContent::Text(config.candidate_prompt.clone())),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                    ModelMessage {
                        role: "user".to_string(),
                        content: Some(MessageContent::Text(user_content.to_string())),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                ],
                tools: vec![],
                max_tokens: Some(2000),
                stream: false,
                response_format: Some(json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "betterclaw_memory_distill",
                        "strict": true,
                        "schema": {
                            "type": "object",
                            "properties": {
                                "wake_pack": { "type": "string" },
                                "facts": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "fact_id": { "type": "string" },
                                            "text": { "type": "string" },
                                            "citations": {
                                                "type": "array",
                                                "items": { "type": "string" }
                                            },
                                            "support_excerpt": { "type": ["string", "null"] },
                                            "falsifier": { "type": ["string", "null"] }
                                        },
                                        "required": ["fact_id", "text", "citations"],
                                        "additionalProperties": false
                                    }
                                },
                                "summary": { "type": ["string", "null"] },
                                "invariant_adds": {
                                    "type": "array",
                                    "items": { "$ref": "#/$defs/item" }
                                },
                                "invariant_removes": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "policies": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "preferences": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "hypotheses": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "drift_flags": {
                                    "type": "array",
                                    "items": { "$ref": "#/$defs/drift_item" }
                                },
                                "drift_contradictions": {
                                    "type": "array",
                                    "items": { "$ref": "#/$defs/drift_item" }
                                },
                                "drift_merges": {
                                    "type": "array",
                                    "items": { "$ref": "#/$defs/drift_item" }
                                }
                            },
                            "required": ["wake_pack", "facts", "invariant_adds", "invariant_removes", "policies", "preferences", "hypotheses", "drift_flags", "drift_contradictions", "drift_merges"],
                            "$defs": {
                                "item": {
                                    "type": "object",
                                    "properties": {
                                        "text": { "type": "string" },
                                        "citations": {
                                            "type": "array",
                                            "items": { "type": "string" }
                                        },
                                        "support_excerpt": { "type": ["string", "null"] },
                                        "falsifier": { "type": ["string", "null"] },
                                        "why_it_holds": { "type": ["string", "null"] },
                                        "supersedes_ids": {
                                            "type": "array",
                                            "items": { "type": "string" }
                                        },
                                        "derived_from_fact_ids": {
                                            "type": "array",
                                            "items": { "type": "string" }
                                        }
                                    },
                                    "required": ["text", "citations"],
                                    "additionalProperties": false
                                },
                                "drift_item": {
                                    "type": "object",
                                    "properties": {
                                        "text": { "type": "string" },
                                        "citations": {
                                            "type": "array",
                                            "items": { "type": "string" }
                                        }
                                    },
                                    "required": ["text", "citations"],
                                    "additionalProperties": false
                                }
                            },
                            "additionalProperties": false
                        }
                    }
                })),
                extra: config.compressor.hyperparams.clone().unwrap_or_else(|| json!({})),
            };

            let compressor_result = compressor_engine.run(compressor_request).await;
            let output_text = match compressor_result {
                Ok(res) => {
                    res.content.clone()
                        .or(res.reasoning.clone())
                        .unwrap_or_else(|| "No content returned.".to_string())
                }
                Err(e) => format!("Error: {:?}", e),
            };

            intermediate_results.push(IntermediateScenario {
                scenario: scenario.clone(),
                compressor_output: output_text,
            });
        }

        let intermediate_path = "eval_intermediate.json";
        fs::write(intermediate_path, serde_json::to_string_pretty(&intermediate_results)?)?;
        println!("Compressor results saved to {}", intermediate_path);

        if mode == "compressor" {
            println!("Compressor mode complete. Swap models if needed, then run with --mode evaluator.");
            return Ok(());
        }
    }

    if mode == "evaluator" || mode == "both" {
        let evaluator_engine = build_engine(&config.evaluator)?;
        let intermediate_path = "eval_intermediate.json";
        let intermediate_str = fs::read_to_string(intermediate_path).context("Failed to read intermediate results. Did you run --mode compressor?")?;
        let results: Vec<IntermediateScenario> = serde_json::from_str(&intermediate_str)?;

        let mut scenario_reports = Vec::new();
        let mut total_score = 0.0;

        for res in results {
            println!("Evaluating scenario: {}", res.scenario.name);

            let default_eval_prompt = "You are an expert evaluator grading an AI's memory compression output against a rubric.\n\nRUBRIC:\n{}\n\nReturn a valid JSON with 'score' (number) and 'reasoning' (string).";
            let template = config.evaluator_prompt.as_deref().unwrap_or(default_eval_prompt);
            let eval_sys_prompt = template.replace("{}", &config.rubric);

            let eval_user_prompt = format!(
                "SCENARIO DATA:\n{}\n\nCOMPRESSOR OUTPUT:\n{}",
                serde_json::to_string_pretty(&res.scenario).unwrap(),
                res.compressor_output
            );

            let evaluator_request = ModelExchangeRequest { role: None,
                model: config.evaluator.model.clone(),
                messages: vec![
                    ModelMessage {
                        role: "system".to_string(),
                        content: Some(MessageContent::Text(eval_sys_prompt)),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                    ModelMessage {
                        role: "user".to_string(),
                        content: Some(MessageContent::Text(eval_user_prompt)),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                ],
                tools: vec![],
                max_tokens: Some(1000),
                stream: false,
                response_format: Some(json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "evaluator_grading",
                        "strict": true,
                        "schema": {
                            "type": "object",
                            "properties": {
                                "score": { "type": "number", "description": "Score from 1 to 10" },
                                "reasoning": { "type": "string" }
                            },
                            "required": ["score", "reasoning"],
                            "additionalProperties": false
                        }
                    }
                })),
                extra: config.evaluator.hyperparams.clone().unwrap_or_else(|| json!({})),
            };

            let evaluator_result = evaluator_engine.run(evaluator_request).await;
            let eval_output = match evaluator_result {
                Ok(res) => {
                    let content = res.content.clone().unwrap_or_default();
                    let reasoning = res.reasoning.clone().unwrap_or_default();
                    
                    // Try parsing content first, fallback to reasoning
                    serde_json::from_str::<EvaluatorOutput>(&content)
                        .or_else(|_| serde_json::from_str::<EvaluatorOutput>(&reasoning))
                        .unwrap_or_else(|e| {
                            println!("Failed to parse evaluator JSON: {}", e);
                            EvaluatorOutput { 
                                score: 0.0, 
                                reasoning: format!("Parse failure (Content: '{}', Reasoning: '{}')", content, reasoning) 
                            }
                        })
                }
                Err(e) => {
                    println!("Evaluator error: {:?}", e);
                    EvaluatorOutput { score: 0.0, reasoning: format!("Evaluator error: {:?}", e) }
                }
            };

            println!("Score: {}", eval_output.score);
            total_score += eval_output.score;

            scenario_reports.push(ScenarioReport {
                scenario_name: res.scenario.name.clone(),
                compressor_raw_response: Some(res.compressor_output),
                score: eval_output.score,
                reasoning: eval_output.reasoning,
            });
        }

        let avg_score = if !scenario_reports.is_empty() {
            total_score / scenario_reports.len() as f64
        } else {
            0.0
        };

        let report = EvalReport {
            timestamp: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
            compressor_model: config.compressor.model.clone(),
            evaluator_model: config.evaluator.model.clone(),
            candidate_prompt: config.candidate_prompt.clone(),
            scenario_reports,
            average_score: avg_score,
        };

        let report_json = serde_json::to_string(&report)?;
        println!("\nFinal Average Score: {}", avg_score);

        let db_path = "prompt_eval_db.jsonl";
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(db_path)?;
        use std::io::Write;
        writeln!(file, "{}", report_json)?;
        println!("Report appended to {}", db_path);
    }

    Ok(())
}
