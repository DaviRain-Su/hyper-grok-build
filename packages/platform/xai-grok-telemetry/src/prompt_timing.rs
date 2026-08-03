//! Per-turn prompt latency measurement.
//!
//! Extracted from `xai-grok-shell::session::prompt_timing`.

use std::time::Instant;

use crate::events::PromptLatency;
use crate::session_ctx::log_event;

pub use crate::enums::McpInitStrategy;

pub struct PromptTiming {
    turn_start: Instant,
    mcp_wait_ms: u64,
    tool_collection_ms: u64,
}

impl PromptTiming {
    pub fn start() -> Self {
        Self {
            turn_start: Instant::now(),
            mcp_wait_ms: 0,
            tool_collection_ms: 0,
        }
    }

    pub fn record_tool_prep(&mut self, mcp_wait_ms: u64, total_prep_ms: u64) {
        self.mcp_wait_ms = mcp_wait_ms;
        self.tool_collection_ms = total_prep_ms.saturating_sub(mcp_wait_ms);
    }

    pub fn emit(
        self,
        model_call_ms: u64,
        turn_index: u32,
        mcp_server_count: u32,
        mcp_tools_registered: u32,
        mcp_strategy: McpInitStrategy,
        model_id: String,
    ) {
        let total_ms = self.turn_start.elapsed().as_millis() as u64;
        let pre_model_ms = total_ms.saturating_sub(model_call_ms);

        log_event(PromptLatency {
            turn_index,
            total_ms,
            mcp_wait_ms: self.mcp_wait_ms,
            tool_collection_ms: self.tool_collection_ms,
            model_call_ms,
            pre_model_ms,
            mcp_server_count,
            mcp_tools_registered,
            mcp_strategy,
            model_id,
        });
    }
}
