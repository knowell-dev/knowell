//! [`KnowellTools`]: the engine behind the MCP server, one method per tool.

use std::future::Future;

use crate::caller::Caller;
use crate::error::ToolError;
use crate::tools::{
    AnalyzeImpactInput, AnalyzeImpactOutput, BuildContextInput, BuildContextOutput, ContractsInput,
    ContractsOutput, FetchInput, FetchOutput, HistoryInput, HistoryOutput, IndexStatusInput,
    IndexStatusOutput, InspectSymbolInput, InspectSymbolOutput, OpenWorkspaceInput,
    OpenWorkspaceOutput, ReadMemoryInput, ReadMemoryOutput, ResumeTaskInput, ResumeTaskOutput,
    SaveCheckpointInput, SaveCheckpointOutput, SearchInput, SearchOutput, TraceFlowInput,
    TraceFlowOutput, WriteMemoryInput, WriteMemoryOutput,
};

/// The engine behind the MCP server: one method per tool.
///
/// The server adapter deserialises and validates arguments
/// ([`crate::Validate`]) before calling a method, resolves the [`Caller`],
/// and renders the result. Implementations:
///
/// - act only on the explicit target in the input (`context_id`, or
///   workspace + view pins); they keep no per-session selection, so
///   concurrent agents never change each other's view;
/// - enforce the caller's permissions, and report an invisible item as
///   [`ToolError::NotFound`] rather than revealing it exists;
/// - attach [`crate::Evidence`] to every source-derived item and explain
///   every empty or partial result with [`crate::Gap`]s; never substitute
///   another ref, profile or index for a missing one;
/// - return repository and memory text as [`crate::UntrustedText`], after
///   secret scanning and redaction;
/// - return a [`crate::JobRef`] for operations that would take longer than
///   a few seconds, and accept the job id on a later call;
/// - keep error messages short and free of secrets and internal detail; use
///   [`ToolError::Internal`] for anything else (it is logged, not sent).
pub trait KnowellTools: Send + Sync + 'static {
    /// `open_workspace`: start-up pack and a new `context_id`.
    fn open_workspace(
        &self,
        caller: &Caller,
        input: OpenWorkspaceInput,
    ) -> impl Future<Output = Result<OpenWorkspaceOutput, ToolError>> + Send;

    /// `search`: hybrid search over code, docs, contracts and memory.
    fn search(
        &self,
        caller: &Caller,
        input: SearchInput,
    ) -> impl Future<Output = Result<SearchOutput, ToolError>> + Send;

    /// `fetch`: versioned source by result id or path.
    fn fetch(
        &self,
        caller: &Caller,
        input: FetchInput,
    ) -> impl Future<Output = Result<FetchOutput, ToolError>> + Send;

    /// `inspect_symbol`: definition, signature, references, implementations, tests.
    fn inspect_symbol(
        &self,
        caller: &Caller,
        input: InspectSymbolInput,
    ) -> impl Future<Output = Result<InspectSymbolOutput, ToolError>> + Send;

    /// `trace_flow`: evidenced relations across projects.
    fn trace_flow(
        &self,
        caller: &Caller,
        input: TraceFlowInput,
    ) -> impl Future<Output = Result<TraceFlowOutput, ToolError>> + Send;

    /// `analyze_impact`: impact of a symbol, file, diff or unapplied patch.
    fn analyze_impact(
        &self,
        caller: &Caller,
        input: AnalyzeImpactInput,
    ) -> impl Future<Output = Result<AnalyzeImpactOutput, ToolError>> + Send;

    /// `contracts`: cross-project contracts, participants and drift.
    fn contracts(
        &self,
        caller: &Caller,
        input: ContractsInput,
    ) -> impl Future<Output = Result<ContractsOutput, ToolError>> + Send;

    /// `build_context`: budgeted source pack for a task.
    fn build_context(
        &self,
        caller: &Caller,
        input: BuildContextInput,
    ) -> impl Future<Output = Result<BuildContextOutput, ToolError>> + Send;

    /// `history`: blame, commits, co-changes and rationale.
    fn history(
        &self,
        caller: &Caller,
        input: HistoryInput,
    ) -> impl Future<Output = Result<HistoryOutput, ToolError>> + Send;

    /// `read_memory`: scoped memory read.
    fn read_memory(
        &self,
        caller: &Caller,
        input: ReadMemoryInput,
    ) -> impl Future<Output = Result<ReadMemoryOutput, ToolError>> + Send;

    /// `write_memory`: store a proposal or record (by permission).
    fn write_memory(
        &self,
        caller: &Caller,
        input: WriteMemoryInput,
    ) -> impl Future<Output = Result<WriteMemoryOutput, ToolError>> + Send;

    /// `resume_task`: list tasks or resume one.
    fn resume_task(
        &self,
        caller: &Caller,
        input: ResumeTaskInput,
    ) -> impl Future<Output = Result<ResumeTaskOutput, ToolError>> + Send;

    /// `save_checkpoint`: task progress and decisions.
    fn save_checkpoint(
        &self,
        caller: &Caller,
        input: SaveCheckpointInput,
    ) -> impl Future<Output = Result<SaveCheckpointOutput, ToolError>> + Send;

    /// `index_status`: freshness, coverage and jobs.
    fn index_status(
        &self,
        caller: &Caller,
        input: IndexStatusInput,
    ) -> impl Future<Output = Result<IndexStatusOutput, ToolError>> + Send;
}
