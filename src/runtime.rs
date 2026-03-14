use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use serde_json::json;

use crate::agent::Agent;
use crate::channel::{Channel, ChannelDefinition};
use crate::event::{Event, EventKind};
use crate::thread::Thread;
use crate::tool::{Tool, ToolDefinition};
use crate::workspace::Workspace;

pub struct Runtime {
    agents: HashMap<String, Agent>,
    workspaces: HashMap<String, Workspace>,
    threads: HashMap<String, Thread>,
    tools: HashMap<String, Box<dyn Tool>>,
    channels: HashMap<String, Box<dyn Channel>>,
    events: Vec<Event>,
}

impl Runtime {
    pub fn new() -> Self {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let workspace = Workspace::new("default", cwd);
        let agent = Agent::new("default", "Default Agent", workspace.id.clone());

        Self {
            agents: HashMap::from([(agent.id.clone(), agent)]),
            workspaces: HashMap::from([(workspace.id.clone(), workspace)]),
            threads: HashMap::new(),
            tools: HashMap::new(),
            channels: HashMap::new(),
            events: Vec::new(),
        }
    }

    pub fn agents(&self) -> &HashMap<String, Agent> {
        &self.agents
    }

    pub fn workspaces(&self) -> &HashMap<String, Workspace> {
        &self.workspaces
    }

    pub fn threads(&self) -> &HashMap<String, Thread> {
        &self.threads
    }

    pub fn tools(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn channels(&self) -> Vec<ChannelDefinition> {
        self.channels
            .values()
            .map(|channel| channel.definition())
            .collect()
    }

    pub fn register_tool(&mut self, tool: Box<dyn Tool>) {
        let definition = tool.definition();
        tracing::info!(tool = %definition.name, "Registered tool");
        self.tools.insert(definition.name.clone(), tool);
    }

    pub fn register_channel(&mut self, channel: Box<dyn Channel>) {
        let definition = channel.definition();
        tracing::info!(channel = %definition.name, "Registered channel");
        self.channels.insert(definition.name.clone(), channel);
    }

    pub fn open_thread(
        &mut self,
        agent_id: impl Into<String>,
        channel: impl Into<String>,
        external_thread_id: Option<String>,
    ) -> String {
        let thread = Thread::new(agent_id.into(), channel.into(), external_thread_id);
        let thread_id = thread.id.to_string();
        tracing::info!(thread_id, "Opened thread");
        self.threads.insert(thread_id.clone(), thread);
        thread_id
    }

    pub fn record_event(&mut self, thread_id: &str, kind: EventKind, payload: serde_json::Value) {
        if let Some(thread) = self.threads.get(thread_id) {
            let event = Event::new(thread.id, kind.clone(), payload);
            tracing::debug!(
                thread_id,
                kind = ?kind,
                event_id = %event.id,
                "Recorded event"
            );
            self.events.push(event);
        } else {
            tracing::warn!(thread_id, "Skipping event for unknown thread");
        }
    }

    pub fn bootstrap_example_state(&mut self) {
        let thread_id = self.open_thread("default", "web", None);
        self.record_event(
            &thread_id,
            EventKind::InboundMessage,
            json!({"content": "hello", "source": "bootstrap"}),
        );
    }
}
