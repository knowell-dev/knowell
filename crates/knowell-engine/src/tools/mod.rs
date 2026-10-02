//! `impl knowell_mcp::KnowellTools for Engine`: the 14 agent tools over
//! real data. Each method resolves the caller to an [`Access`], runs the
//! tool and records usage counters.

mod code;
mod context;
mod memory;
mod relations;
mod workspace;

use std::future::Future;
use std::time::Instant;

use knowell_mcp::tools::{
    AnalyzeImpactInput, AnalyzeImpactOutput, BuildContextInput, BuildContextOutput, ContractsInput,
    ContractsOutput, FetchInput, FetchOutput, HistoryInput, HistoryOutput, IndexStatusInput,
    IndexStatusOutput, InspectSymbolInput, InspectSymbolOutput, OpenWorkspaceInput,
    OpenWorkspaceOutput, ReadMemoryInput, ReadMemoryOutput, ResumeTaskInput, ResumeTaskOutput,
    SaveCheckpointInput, SaveCheckpointOutput, SearchInput, SearchOutput, TraceFlowInput,
    TraceFlowOutput, WriteMemoryInput, WriteMemoryOutput,
};
use knowell_mcp::{Caller, KnowellTools, ToolError, ToolName};
use serde::Serialize;
use time::OffsetDateTime;

use crate::access::Access;
use crate::engine::Engine;
use crate::usage::CallRecord;

pub(crate) use memory::{conflict_map, memory_record, now_timestamp};

impl Engine {
    /// Resolves the caller, runs `run`, and records the call.
    async fn instrument<T, F, Fut>(
        &self,
        tool: ToolName,
        caller: &Caller,
        run: F,
    ) -> Result<T, ToolError>
    where
        T: Serialize,
        F: FnOnce(Access) -> Fut,
        Fut: Future<Output = Result<T, ToolError>>,
    {
        let started = Instant::now();
        let result = match self.inner.access.resolve(caller) {
            Ok(access) => run(access).await,
            Err(error) => Err(error),
        };
        if let Err(ToolError::Internal(detail)) = &result {
            tracing::error!(tool = tool.as_str(), detail = %detail, "tool failed");
        }
        let output_bytes = match &result {
            Ok(value) => serde_json::to_vec(value).map_or(0, |b| b.len()),
            Err(_) => 0,
        };
        let agent = caller
            .client
            .as_ref()
            .map_or("mcp-client", |c| c.name.as_str());
        self.inner.usage.record(&CallRecord {
            tool: tool.as_str(),
            agent,
            session: agent,
            ok: result.is_ok(),
            output_bytes,
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            at: OffsetDateTime::now_utc(),
        });
        result
    }
}

impl KnowellTools for Engine {
    async fn open_workspace(
        &self,
        caller: &Caller,
        input: OpenWorkspaceInput,
    ) -> Result<OpenWorkspaceOutput, ToolError> {
        self.instrument(ToolName::OpenWorkspace, caller, |access| {
            self.tool_open_workspace(access, input)
        })
        .await
    }

    async fn search(&self, caller: &Caller, input: SearchInput) -> Result<SearchOutput, ToolError> {
        self.instrument(ToolName::Search, caller, |access| {
            self.tool_search(access, input)
        })
        .await
    }

    async fn fetch(&self, caller: &Caller, input: FetchInput) -> Result<FetchOutput, ToolError> {
        self.instrument(ToolName::Fetch, caller, |access| {
            self.tool_fetch(access, input)
        })
        .await
    }

    async fn inspect_symbol(
        &self,
        caller: &Caller,
        input: InspectSymbolInput,
    ) -> Result<InspectSymbolOutput, ToolError> {
        self.instrument(ToolName::InspectSymbol, caller, |access| {
            self.tool_inspect_symbol(access, input)
        })
        .await
    }

    async fn trace_flow(
        &self,
        caller: &Caller,
        input: TraceFlowInput,
    ) -> Result<TraceFlowOutput, ToolError> {
        self.instrument(ToolName::TraceFlow, caller, |access| {
            self.tool_trace_flow(access, input)
        })
        .await
    }

    async fn analyze_impact(
        &self,
        caller: &Caller,
        input: AnalyzeImpactInput,
    ) -> Result<AnalyzeImpactOutput, ToolError> {
        self.instrument(ToolName::AnalyzeImpact, caller, |access| {
            self.tool_analyze_impact(access, input)
        })
        .await
    }

    async fn contracts(
        &self,
        caller: &Caller,
        input: ContractsInput,
    ) -> Result<ContractsOutput, ToolError> {
        self.instrument(ToolName::Contracts, caller, |access| {
            self.tool_contracts(access, input)
        })
        .await
    }

    async fn build_context(
        &self,
        caller: &Caller,
        input: BuildContextInput,
    ) -> Result<BuildContextOutput, ToolError> {
        self.instrument(ToolName::BuildContext, caller, |access| {
            self.tool_build_context(access, input)
        })
        .await
    }

    async fn history(
        &self,
        caller: &Caller,
        input: HistoryInput,
    ) -> Result<HistoryOutput, ToolError> {
        self.instrument(ToolName::History, caller, |access| {
            self.tool_history(access, input)
        })
        .await
    }

    async fn read_memory(
        &self,
        caller: &Caller,
        input: ReadMemoryInput,
    ) -> Result<ReadMemoryOutput, ToolError> {
        self.instrument(ToolName::ReadMemory, caller, |access| {
            self.tool_read_memory(access, input)
        })
        .await
    }

    async fn write_memory(
        &self,
        caller: &Caller,
        input: WriteMemoryInput,
    ) -> Result<WriteMemoryOutput, ToolError> {
        self.instrument(ToolName::WriteMemory, caller, |access| {
            self.tool_write_memory(access, input)
        })
        .await
    }

    async fn resume_task(
        &self,
        caller: &Caller,
        input: ResumeTaskInput,
    ) -> Result<ResumeTaskOutput, ToolError> {
        self.instrument(ToolName::ResumeTask, caller, |access| {
            self.tool_resume_task(access, input)
        })
        .await
    }

    async fn save_checkpoint(
        &self,
        caller: &Caller,
        input: SaveCheckpointInput,
    ) -> Result<SaveCheckpointOutput, ToolError> {
        self.instrument(ToolName::SaveCheckpoint, caller, |access| {
            self.tool_save_checkpoint(access, input)
        })
        .await
    }

    async fn index_status(
        &self,
        caller: &Caller,
        input: IndexStatusInput,
    ) -> Result<IndexStatusOutput, ToolError> {
        self.instrument(ToolName::IndexStatus, caller, |access| {
            self.tool_index_status(access, input)
        })
        .await
    }
}
