//! Workflow DAGs — a directed acyclic graph of pipeline steps.
//!
//! [`PipelineDefinition`](super::PipelineDefinition) is strictly
//! linear: one byte stream flows through an ordered step list. Real
//! workflows often fan out — validate once, then run several
//! independent inspections or transforms in parallel — which a linear
//! list cannot express. [`DagDefinition`] describes that shape: named
//! nodes (each wrapping one [`PipelineStep`]) connected by directed
//! [`DagEdge`]s.
//!
//! # Semantics
//!
//! - **Data flow is fan-out, not wired.** Every node executes its
//!   step against the *same* input bytes supplied to
//!   [`run_dag`]. Edges express *dependency and failure propagation
//!   only* — they do not route one node's output artifact into
//!   another node. To chain byte transforms, use a linear
//!   [`PipelineDefinition`]; multi-input operations already accept
//!   inline `other_input(s)` params for that purpose.
//! - **Execution is wave-by-wave.** [`DagDefinition::execution_waves`]
//!   groups nodes into topological levels via Kahn's algorithm; each
//!   wave's nodes are mutually independent and *could* run in
//!   parallel. [`run_dag`] currently executes them sequentially in a
//!   deterministic order (node ids sorted within each wave), which is
//!   safe because nodes share no mutable state.
//! - **Failure policy: skip dependents, continue siblings.** A node
//!   whose step fails is recorded [`DagNodeStatus::Failed`]; every
//!   node with a transitive dependency on it is
//!   [`DagNodeStatus::Skipped`] without executing. Nodes on
//!   independent branches still run. This differs deliberately from
//!   [`PipelineEngine::run_pipeline`](super::PipelineEngine::run_pipeline),
//!   which aborts the whole run on the first failure — a DAG run
//!   always produces a complete [`DagReport`].
//! - **Limits.** Each node runs as a single-step
//!   [`PipelineDefinition`] through the existing executor, so
//!   per-step input size, page/output budgets, and parameter
//!   validation are enforced unchanged. [`PipelineLimits::max_steps`]
//!   is applied to the node count, and a DAG-wide cumulative byte
//!   budget approximates `max_total_bytes`: the input plus each
//!   succeeded node's *changed* output artifact (report steps that
//!   pass bytes through count as 0). A node that would exceed the
//!   cumulative budget fails like any other node error.
//!
//! # Limitations
//!
//! - No data-dependent routing: a node cannot consume another node's
//!   artifact, so diamond joins see the original input, not merged
//!   branch outputs.
//! - Sequential execution only — waves are computed for parallelism
//!   but `run_dag` does not spawn tasks.
//! - `max_total_bytes` across the DAG is an approximation (see
//!   above); per-node limits are exact.
//! - Node steps are validated lazily: an unknown engine/operation is
//!   reported as that node's [`DagNodeStatus::Failed`], not as a
//!   definition error, because step validation lives inside the
//!   executor. [`DagDefinition::validate`] covers graph shape only.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::PipelineEngine;
use super::model::{PipelineArtifact, PipelineDefinition, PipelineStep};

/// Maximum nodes a [`DagDefinition`] may contain.
pub const MAX_DAG_NODES: usize = 64;

/// Maximum edges a [`DagDefinition`] may contain.
pub const MAX_DAG_EDGES: usize = 256;

/// One node in a [`DagDefinition`]: a stable id plus the
/// [`PipelineStep`] to execute when the node runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DagNode {
    /// Unique node identifier referenced by [`DagEdge`] endpoints.
    /// Must be non-empty and unique within the definition.
    pub id: String,
    /// The pipeline step this node executes. `params` live inside
    /// the step exactly as in a linear pipeline.
    pub step: PipelineStep,
}

impl DagNode {
    /// Convenience constructor.
    pub fn new(id: impl Into<String>, step: PipelineStep) -> Self {
        Self {
            id: id.into(),
            step,
        }
    }
}

/// A directed dependency edge `from -> to`: `to` does not run until
/// `from` has succeeded. Both endpoints must be node ids in the same
/// definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DagEdge {
    /// Source node id — the dependency.
    pub from: String,
    /// Target node id — the dependent.
    pub to: String,
}

impl DagEdge {
    /// Convenience constructor.
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }
}

/// A named DAG of [`DagNode`]s connected by [`DagEdge`]s.
///
/// Pure serde data like [`PipelineDefinition`]: construct in code or
/// deserialize from JSON/YAML. Call [`validate`](Self::validate)
/// before executing; [`run_dag`] validates implicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DagDefinition {
    /// Stable identifier, e.g. `"pdf.review"`. Used as the `dag`
    /// field of [`DagReport`] and to namespace per-node pipeline
    /// names internally.
    pub name: String,
    /// Optional human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Graph nodes. 1..=[`MAX_DAG_NODES`] entries with unique,
    /// non-empty ids.
    pub nodes: Vec<DagNode>,
    /// Dependency edges. At most [`MAX_DAG_EDGES`] entries; every
    /// endpoint must name a node.
    #[serde(default)]
    pub edges: Vec<DagEdge>,
}

impl DagDefinition {
    /// Convenience constructor for an empty graph (add nodes/edges
    /// via the public fields).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Validate the graph shape: size limits, unique non-empty node
    /// ids, no dangling edge endpoints, and acyclicity.
    ///
    /// Uses Kahn's algorithm; when nodes remain unprocessed the error
    /// names the nodes that lie on a cycle (computed exactly, not
    /// merely "unreachable from a source").
    ///
    /// # Errors
    /// [`DagError::Empty`], [`DagError::TooManyNodes`],
    /// [`DagError::TooManyEdges`], [`DagError::EmptyNodeId`],
    /// [`DagError::DuplicateId`], [`DagError::DanglingEndpoint`], or
    /// [`DagError::Cycle`].
    pub fn validate(&self) -> Result<(), DagError> {
        if self.nodes.is_empty() {
            return Err(DagError::Empty(self.name.clone()));
        }
        if self.nodes.len() > MAX_DAG_NODES {
            return Err(DagError::TooManyNodes {
                count: self.nodes.len(),
                max: MAX_DAG_NODES,
            });
        }
        if self.edges.len() > MAX_DAG_EDGES {
            return Err(DagError::TooManyEdges {
                count: self.edges.len(),
                max: MAX_DAG_EDGES,
            });
        }

        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            if node.id.is_empty() {
                return Err(DagError::EmptyNodeId);
            }
            if !ids.insert(node.id.as_str()) {
                return Err(DagError::DuplicateId(node.id.clone()));
            }
        }

        for (index, edge) in self.edges.iter().enumerate() {
            if !ids.contains(edge.from.as_str()) {
                return Err(DagError::DanglingEndpoint {
                    edge: index,
                    endpoint: "from",
                    id: edge.from.clone(),
                });
            }
            if !ids.contains(edge.to.as_str()) {
                return Err(DagError::DanglingEndpoint {
                    edge: index,
                    endpoint: "to",
                    id: edge.to.clone(),
                });
            }
        }

        // Kahn's algorithm: repeatedly remove zero-indegree nodes.
        // Anything left belongs to a cycle or depends on one.
        let (mut indegree, adj) = self.graph_maps();
        let mut ready: VecDeque<String> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| id.clone())
            .collect();
        let mut processed = 0usize;
        while let Some(id) = ready.pop_front() {
            processed += 1;
            if let Some(next) = adj.get(id.as_str()) {
                for dep in next {
                    if let Some(d) = indegree.get_mut(dep) {
                        *d -= 1;
                        if *d == 0 {
                            ready.push_back(dep.clone());
                        }
                    }
                }
            }
        }
        if processed != self.nodes.len() {
            let remaining: BTreeSet<String> = indegree
                .iter()
                .filter(|(_, d)| **d > 0)
                .map(|(id, _)| id.clone())
                .collect();
            return Err(DagError::Cycle {
                nodes: self.cycle_members(&remaining, &adj),
            });
        }
        Ok(())
    }

    /// Group node ids into parallel-safe execution waves via Kahn's
    /// algorithm by levels: wave 0 holds all zero-indegree nodes,
    /// wave *k* the nodes whose dependencies all finished in earlier
    /// waves. Each wave's ids are **sorted**, so the result is
    /// deterministic regardless of definition ordering.
    ///
    /// # Errors
    /// Whatever [`validate`](Self::validate) returns.
    pub fn execution_waves(&self) -> Result<Vec<Vec<String>>, DagError> {
        self.validate()?;
        let (mut indegree, adj) = self.graph_maps();
        let mut ready: BTreeSet<String> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| id.clone())
            .collect();
        let mut waves = Vec::new();
        while !ready.is_empty() {
            let wave: Vec<String> = ready.iter().cloned().collect();
            let mut next_ready = BTreeSet::new();
            for id in &wave {
                indegree.remove(id);
                if let Some(deps) = adj.get(id.as_str()) {
                    for dep in deps {
                        if let Some(d) = indegree.get_mut(dep) {
                            *d -= 1;
                            if *d == 0 {
                                next_ready.insert(dep.clone());
                            }
                        }
                    }
                }
            }
            ready = next_ready;
            waves.push(wave);
        }
        Ok(waves)
    }

    /// Execute this DAG against `input` using `engine`.
    ///
    /// Equivalent to [`run_dag`]; see its documentation for the full
    /// semantics (fan-out input, wave order, skip-on-failure policy,
    /// limits).
    ///
    /// # Errors
    /// [`DagError`] when the graph fails [`validate`](Self::validate)
    /// or exceeds the engine's `max_steps` node budget. Individual
    /// node failures are *not* errors — they appear as
    /// [`DagNodeStatus::Failed`] entries in the report.
    pub fn run(&self, engine: &PipelineEngine, input: &[u8]) -> Result<DagReport, DagError> {
        run_dag(engine, self, input)
    }

    /// Adjacency (`from -> [to]`) and indegree maps over node ids.
    /// Callers must have validated endpoints already.
    fn graph_maps(&self) -> (BTreeMap<String, usize>, BTreeMap<String, Vec<String>>) {
        let mut indegree: BTreeMap<String, usize> =
            self.nodes.iter().map(|n| (n.id.clone(), 0usize)).collect();
        let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for edge in &self.edges {
            *indegree.entry(edge.to.clone()).or_insert(0) += 1;
            adj.entry(edge.from.clone())
                .or_default()
                .push(edge.to.clone());
        }
        (indegree, adj)
    }

    /// Of the `remaining` nodes (those Kahn could not process),
    /// return exactly those that lie on a directed cycle: a node is
    /// on a cycle iff it can reach itself following edges that stay
    /// inside `remaining`. Pure dependents of a cycle are excluded.
    fn cycle_members(
        &self,
        remaining: &BTreeSet<String>,
        adj: &BTreeMap<String, Vec<String>>,
    ) -> Vec<String> {
        let mut members = Vec::new();
        for start in remaining {
            // DFS from start's successors; reaching start again means
            // start sits on a cycle.
            let mut stack: Vec<&str> = adj
                .get(start.as_str())
                .map(|v| v.iter().map(String::as_str).collect())
                .unwrap_or_default();
            let mut seen: HashSet<&str> = HashSet::new();
            let mut on_cycle = false;
            while let Some(node) = stack.pop() {
                if node == start.as_str() {
                    on_cycle = true;
                    break;
                }
                if !seen.insert(node) {
                    continue;
                }
                if remaining.contains(node) {
                    if let Some(next) = adj.get(node) {
                        stack.extend(next.iter().map(String::as_str));
                    }
                }
            }
            if on_cycle {
                members.push(start.clone());
            }
        }
        members.sort();
        members
    }
}

/// Errors from [`DagDefinition::validate`] / [`run_dag`] graph
/// checks. Node-level execution failures never surface here — they
/// are recorded in [`DagNodeReport`].
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    /// The definition contains no nodes.
    #[error("dag `{0}` has no nodes")]
    Empty(String),

    /// More than [`MAX_DAG_NODES`] nodes.
    #[error("dag has {count} nodes, exceeding the limit of {max}")]
    TooManyNodes {
        /// Actual node count.
        count: usize,
        /// [`MAX_DAG_NODES`].
        max: usize,
    },

    /// More than [`MAX_DAG_EDGES`] edges.
    #[error("dag has {count} edges, exceeding the limit of {max}")]
    TooManyEdges {
        /// Actual edge count.
        count: usize,
        /// [`MAX_DAG_EDGES`].
        max: usize,
    },

    /// A node id is the empty string.
    #[error("dag contains a node with an empty id")]
    EmptyNodeId,

    /// Two nodes share the same id.
    #[error("duplicate dag node id `{0}`")]
    DuplicateId(String),

    /// An edge endpoint does not name a node in the definition.
    #[error("dag edge {edge} has a dangling `{endpoint}` endpoint `{id}`")]
    DanglingEndpoint {
        /// 0-based edge index.
        edge: usize,
        /// Which endpoint: `"from"` or `"to"`.
        endpoint: &'static str,
        /// The unrecognized node id.
        id: String,
    },

    /// The graph contains a directed cycle. `nodes` lists exactly the
    /// node ids lying on a cycle (sorted); dependents of a cycle are
    /// not included.
    #[error("dag contains a cycle involving nodes: {}", nodes.join(", "))]
    Cycle {
        /// Node ids on a cycle, sorted.
        nodes: Vec<String>,
    },

    /// The engine's [`PipelineLimits::max_steps`](super::PipelineLimits)
    /// budget is smaller than the node count.
    #[error("pipeline resource limit exceeded: {0}")]
    LimitExceeded(String),
}

/// Outcome of one DAG node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DagNodeStatus {
    /// The step ran and produced its artifact/report.
    Succeeded,
    /// The step ran and failed; dependents are skipped.
    Failed,
    /// A transitive dependency failed or was skipped, so this node
    /// never executed.
    Skipped,
}

/// Per-node record in [`DagReport::nodes`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DagNodeReport {
    /// Node id from the definition.
    pub id: String,
    /// Final status of the node.
    pub status: DagNodeStatus,
    /// Human-readable summary: the `engine.operation` that ran, or
    /// the reason the node was skipped.
    pub detail: String,
    /// Error text when `status` is [`DagNodeStatus::Failed`] — the
    /// full [`PipelineError`] display, preserving step context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Structured report produced by report steps (`validate`,
    /// `inspect`, `extract_text`, ...), identical to
    /// [`StepReport::output`](super::StepReport::output). `None` for
    /// byte transforms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// The artifact the node's step produced, when it succeeded. For
    /// report steps this is the input passed through unchanged; for
    /// transforms it is the transformed document (or
    /// [`PipelineArtifact::Multiple`] for split-style ops). Present
    /// so callers can collect per-branch results — the DAG does not
    /// feed these artifacts into other nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<PipelineArtifact>,
}

/// Result of a [`run_dag`] execution. Always returned for a valid
/// graph — node failures are data, not errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DagReport {
    /// Name of the [`DagDefinition`] that ran.
    pub dag: String,
    /// Per-node reports in deterministic execution order: wave by
    /// wave, node ids sorted within each wave.
    pub nodes: Vec<DagNodeReport>,
    /// Nodes that ran successfully.
    pub succeeded: usize,
    /// Nodes whose step failed.
    pub failed: usize,
    /// Nodes skipped because a dependency did not succeed.
    pub skipped: usize,
}

/// Execute `def` against `input` wave-by-wave on `engine`.
///
/// Each node runs its [`PipelineStep`] as a single-step
/// [`PipelineDefinition`] through
/// [`PipelineEngine::run_pipeline`], so all existing step validation
/// and per-node limits apply unchanged. Every node receives the same
/// `input` bytes (fan-out — see module docs). Nodes execute
/// sequentially in the deterministic order given by
/// [`DagDefinition::execution_waves`].
///
/// Failure policy: a failed node marks its transitive dependents
/// [`DagNodeStatus::Skipped`]; independent branches continue. The
/// DAG-wide cumulative byte budget (`max_total_bytes`) is enforced
/// approximately — see module docs.
///
/// # Errors
/// [`DagError::Empty`]/[`DagError::TooManyNodes`]/
/// [`DagError::TooManyEdges`]/[`DagError::EmptyNodeId`]/
/// [`DagError::DuplicateId`]/[`DagError::DanglingEndpoint`]/
/// [`DagError::Cycle`] from graph validation, or
/// [`DagError::LimitExceeded`] when the node count exceeds the
/// engine's `max_steps`.
pub fn run_dag(
    engine: &PipelineEngine,
    def: &DagDefinition,
    input: &[u8],
) -> Result<DagReport, DagError> {
    let waves = def.execution_waves()?;

    if def.nodes.len() as u64 > engine.limits().max_steps as u64 {
        return Err(DagError::LimitExceeded(format!(
            "dag `{}` has {} nodes, exceeding max_steps ({})",
            def.name,
            def.nodes.len(),
            engine.limits().max_steps
        )));
    }

    let nodes_by_id: HashMap<&str, &DagNode> =
        def.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    // Reverse adjacency: node id -> sorted ids of its direct
    // dependencies (edges `dep -> node`).
    let mut predecessors: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &def.edges {
        predecessors
            .entry(edge.to.as_str())
            .or_default()
            .push(edge.from.as_str());
    }
    for deps in predecessors.values_mut() {
        deps.sort_unstable();
        deps.dedup();
    }

    let mut reports: Vec<DagNodeReport> = Vec::with_capacity(def.nodes.len());
    let mut status_by_id: HashMap<&str, DagNodeStatus> = HashMap::new();
    // DAG-wide cumulative byte budget (approximation of
    // max_total_bytes across all branches).
    let mut total_bytes = input.len() as u64;
    engine
        .limits()
        .check_total(total_bytes)
        .map_err(|m| DagError::LimitExceeded(m))?;

    for wave in &waves {
        for id in wave {
            let node = nodes_by_id[id.as_str()];
            // Skip policy: if any direct dependency did not succeed,
            // this node is skipped. Transitivity is automatic — a
            // skipped dependency itself has status Skipped, which is
            // != Succeeded, so the skip propagates wave by wave.
            let blocking = predecessors.get(id.as_str()).and_then(|deps| {
                deps.iter()
                    .filter(|dep| {
                        status_by_id
                            .get(**dep)
                            .is_some_and(|s| *s != DagNodeStatus::Succeeded)
                    })
                    .map(|dep| (*dep).to_string())
                    .next()
            });

            if let Some(dep) = blocking {
                let dep_status = status_by_id[dep.as_str()];
                reports.push(DagNodeReport {
                    id: id.clone(),
                    status: DagNodeStatus::Skipped,
                    detail: format!(
                        "skipped: dependency `{dep}` {}",
                        match dep_status {
                            DagNodeStatus::Failed => "failed",
                            DagNodeStatus::Skipped => "was skipped",
                            DagNodeStatus::Succeeded => unreachable!(),
                        }
                    ),
                    error: None,
                    output: None,
                    artifact: None,
                });
                status_by_id.insert(id.as_str(), DagNodeStatus::Skipped);
                continue;
            }

            // Run the node's step as a single-step pipeline through
            // the existing executor — same validation, same per-step
            // input limit, same engine dispatch.
            let step_def = PipelineDefinition {
                name: format!("{}.{}", def.name, node.id),
                description: None,
                steps: vec![node.step.clone()],
            };
            match engine.run_pipeline(&step_def, input) {
                Ok(out) => {
                    // Approximate produced bytes: a transform changes
                    // the artifact; a report step passes input
                    // through unchanged and produces nothing new.
                    let produced = match out.artifact.as_bytes() {
                        Some(b) if b == input => 0u64,
                        _ => out.artifact.len_bytes(),
                    };
                    total_bytes = total_bytes.saturating_add(produced);
                    let limit_err = engine.limits().check_total(total_bytes).err();
                    if let Some(msg) = limit_err {
                        reports.push(DagNodeReport {
                            id: id.clone(),
                            status: DagNodeStatus::Failed,
                            detail: format!("{}.{}", node.step.engine, node.step.operation),
                            error: Some(format!("dag cumulative limit: {msg}")),
                            output: None,
                            artifact: None,
                        });
                        status_by_id.insert(id.as_str(), DagNodeStatus::Failed);
                    } else {
                        let step_report = out.steps.into_iter().next();
                        reports.push(DagNodeReport {
                            id: id.clone(),
                            status: DagNodeStatus::Succeeded,
                            detail: format!("{}.{}", node.step.engine, node.step.operation),
                            error: None,
                            output: step_report.and_then(|r| r.output),
                            artifact: Some(out.artifact),
                        });
                        status_by_id.insert(id.as_str(), DagNodeStatus::Succeeded);
                    }
                }
                Err(err) => {
                    reports.push(DagNodeReport {
                        id: id.clone(),
                        status: DagNodeStatus::Failed,
                        detail: format!("{}.{}", node.step.engine, node.step.operation),
                        error: Some(err.to_string()),
                        output: None,
                        artifact: None,
                    });
                    status_by_id.insert(id.as_str(), DagNodeStatus::Failed);
                }
            }
        }
    }

    let succeeded = reports
        .iter()
        .filter(|r| r.status == DagNodeStatus::Succeeded)
        .count();
    let failed = reports
        .iter()
        .filter(|r| r.status == DagNodeStatus::Failed)
        .count();
    let skipped = reports
        .iter()
        .filter(|r| r.status == DagNodeStatus::Skipped)
        .count();

    Ok(DagReport {
        dag: def.name.clone(),
        nodes: reports,
        succeeded,
        failed,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pipeline_engine::{PipelineStep, engine_name, operation};

    /// Minimal structurally valid one-page PDF (same fixture as
    /// tests.rs / security-engine tests).
    const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
trailer<</Root 1 0 R/ID[<0123456789abcdef0123456789abcdef><0123456789abcdef0123456789abcdef>]>>\n\
%%EOF";

    fn step(engine: &str, op: &str) -> PipelineStep {
        PipelineStep::new("s", engine, op)
    }

    /// Diamond: a -> {b, c} -> d.
    fn diamond() -> DagDefinition {
        let mut def = DagDefinition::new("test.diamond");
        def.nodes = vec![
            DagNode::new("a", step(engine_name::SECURITY, operation::VALIDATE)),
            DagNode::new("b", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("c", step(engine_name::PDF, operation::PDF_PAGE_COUNT)),
            DagNode::new("d", step(engine_name::PDF, operation::PDF_VALIDATE)),
        ];
        def.edges = vec![
            DagEdge::new("a", "b"),
            DagEdge::new("a", "c"),
            DagEdge::new("b", "d"),
            DagEdge::new("c", "d"),
        ];
        def
    }

    #[test]
    fn diamond_topo_and_waves() {
        let def = diamond();
        def.validate().unwrap();
        let waves = def.execution_waves().unwrap();
        assert_eq!(
            waves,
            vec![
                vec!["a".to_string()],
                vec!["b".to_string(), "c".to_string()],
                vec!["d".to_string()],
            ]
        );
    }

    #[test]
    fn wave_order_is_sorted_regardless_of_definition_order() {
        // Same diamond, nodes and edges declared in scrambled order.
        let mut def = DagDefinition::new("scrambled");
        def.nodes = vec![
            DagNode::new("d", step(engine_name::PDF, operation::PDF_VALIDATE)),
            DagNode::new("c", step(engine_name::PDF, operation::PDF_PAGE_COUNT)),
            DagNode::new("b", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("a", step(engine_name::SECURITY, operation::VALIDATE)),
        ];
        def.edges = vec![
            DagEdge::new("c", "d"),
            DagEdge::new("b", "d"),
            DagEdge::new("a", "c"),
            DagEdge::new("a", "b"),
        ];
        assert_eq!(
            def.execution_waves().unwrap(),
            diamond().execution_waves().unwrap()
        );
    }

    #[test]
    fn rejects_cycle_and_names_cycle_nodes() {
        let mut def = DagDefinition::new("cyclic");
        def.nodes = vec![
            DagNode::new("a", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("b", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("c", step(engine_name::PDF, operation::PDF_INSPECT)),
            // tail depends on the cycle but is not itself on it
            DagNode::new("tail", step(engine_name::PDF, operation::PDF_INSPECT)),
        ];
        def.edges = vec![
            DagEdge::new("a", "b"),
            DagEdge::new("b", "a"),
            DagEdge::new("b", "c"),
            DagEdge::new("c", "b"),
            DagEdge::new("a", "tail"),
        ];
        match def.validate() {
            Err(DagError::Cycle { nodes }) => {
                assert_eq!(
                    nodes,
                    vec!["a".to_string(), "b".to_string(), "c".to_string()]
                );
                assert!(!nodes.contains(&"tail".to_string()));
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
        assert!(def.execution_waves().is_err());
    }

    #[test]
    fn rejects_self_loop_as_cycle() {
        let mut def = DagDefinition::new("self");
        def.nodes = vec![DagNode::new(
            "a",
            step(engine_name::PDF, operation::PDF_INSPECT),
        )];
        def.edges = vec![DagEdge::new("a", "a")];
        assert!(matches!(
            def.validate(),
            Err(DagError::Cycle { nodes }) if nodes == vec!["a".to_string()]
        ));
    }

    #[test]
    fn rejects_dangling_endpoints() {
        let mut def = DagDefinition::new("dangle");
        def.nodes = vec![DagNode::new(
            "a",
            step(engine_name::PDF, operation::PDF_INSPECT),
        )];
        def.edges = vec![DagEdge::new("a", "ghost"), DagEdge::new("phantom", "a")];
        match def.validate() {
            Err(DagError::DanglingEndpoint { edge, endpoint, id }) => {
                assert_eq!(edge, 0);
                assert_eq!(endpoint, "to");
                assert_eq!(id, "ghost");
            }
            other => panic!("expected DanglingEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn rejects_duplicate_and_empty_ids() {
        let mut def = DagDefinition::new("dups");
        def.nodes = vec![
            DagNode::new("a", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("a", step(engine_name::PDF, operation::PDF_INSPECT)),
        ];
        assert!(matches!(
            def.validate(),
            Err(DagError::DuplicateId(id)) if id == "a"
        ));

        def.nodes = vec![
            DagNode::new("", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("b", step(engine_name::PDF, operation::PDF_INSPECT)),
        ];
        assert!(matches!(def.validate(), Err(DagError::EmptyNodeId)));
    }

    #[test]
    fn rejects_oversized_graphs() {
        let mut def = DagDefinition::new("big");
        def.nodes = (0..=MAX_DAG_NODES)
            .map(|i| {
                DagNode::new(
                    format!("n{i}"),
                    step(engine_name::PDF, operation::PDF_INSPECT),
                )
            })
            .collect();
        assert!(matches!(def.validate(), Err(DagError::TooManyNodes { .. })));

        let mut def = DagDefinition::new("many-edges");
        def.nodes = vec![
            DagNode::new("a", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("b", step(engine_name::PDF, operation::PDF_INSPECT)),
        ];
        def.edges = (0..=MAX_DAG_EDGES)
            .map(|_| DagEdge::new("a", "b"))
            .collect();
        assert!(matches!(def.validate(), Err(DagError::TooManyEdges { .. })));

        assert!(matches!(
            DagDefinition::new("empty").validate(),
            Err(DagError::Empty(name)) if name == "empty"
        ));
    }

    #[test]
    fn dag_definition_is_serde_round_trippable() {
        let def = diamond();
        let json = serde_json::to_string_pretty(&def).unwrap();
        let back: DagDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn run_dag_executes_diamond_and_reports() {
        let engine = PipelineEngine::new();
        let report = run_dag(&engine, &diamond(), MINIMAL_PDF).unwrap();

        assert_eq!(report.dag, "test.diamond");
        assert_eq!(report.succeeded, 4);
        assert_eq!(report.failed, 0);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.nodes.len(), 4);

        // Reports follow wave order: a, then sorted {b, c}, then d.
        let order: Vec<&str> = report.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
        for node in &report.nodes {
            assert_eq!(node.status, DagNodeStatus::Succeeded);
            assert!(node.error.is_none());
            assert!(node.artifact.is_some());
        }
        // Report steps carry their structured output.
        let b = report.nodes.iter().find(|n| n.id == "b").unwrap();
        assert!(b.output.as_ref().unwrap()["page_count"].is_number());
    }

    #[test]
    fn failed_node_skips_dependents_but_siblings_continue() {
        let engine = PipelineEngine::new();
        let mut def = DagDefinition::new("fail.branch");
        def.nodes = vec![
            // root gate succeeds
            DagNode::new("gate", step(engine_name::SECURITY, operation::VALIDATE)),
            // failing branch: extract_pages without required `pages`
            DagNode::new(
                "bad",
                PipelineStep::new("bad", engine_name::PDF, operation::PDF_EXTRACT_PAGES),
            ),
            // healthy sibling branch
            DagNode::new("inspect", step(engine_name::PDF, operation::PDF_INSPECT)),
            // join depends on both branches
            DagNode::new("join", step(engine_name::PDF, operation::PDF_VALIDATE)),
            // fully independent root node
            DagNode::new("count", step(engine_name::PDF, operation::PDF_PAGE_COUNT)),
        ];
        def.edges = vec![
            DagEdge::new("gate", "bad"),
            DagEdge::new("gate", "inspect"),
            DagEdge::new("bad", "join"),
            DagEdge::new("inspect", "join"),
        ];

        let report = run_dag(&engine, &def, MINIMAL_PDF).unwrap();
        let status = |id: &str| report.nodes.iter().find(|n| n.id == id).unwrap().status;

        assert_eq!(status("gate"), DagNodeStatus::Succeeded);
        assert_eq!(status("bad"), DagNodeStatus::Failed);
        assert_eq!(status("inspect"), DagNodeStatus::Succeeded);
        // join has a failed transitive dep -> skipped even though
        // `inspect` succeeded.
        assert_eq!(status("join"), DagNodeStatus::Skipped);
        // independent branch unaffected.
        assert_eq!(status("count"), DagNodeStatus::Succeeded);

        assert_eq!(report.succeeded, 3);
        assert_eq!(report.failed, 1);
        assert_eq!(report.skipped, 1);

        let bad = report.nodes.iter().find(|n| n.id == "bad").unwrap();
        assert!(bad.error.is_some());
        let join = report.nodes.iter().find(|n| n.id == "join").unwrap();
        assert!(join.detail.contains("bad"));
        assert!(join.error.is_none());
    }

    #[test]
    fn skipped_propagates_transitively() {
        let engine = PipelineEngine::new();
        let mut def = DagDefinition::new("chain.skip");
        def.nodes = vec![
            DagNode::new(
                "bad",
                PipelineStep::new("bad", engine_name::PDF, operation::PDF_EXTRACT_PAGES),
            ),
            DagNode::new("mid", step(engine_name::PDF, operation::PDF_INSPECT)),
            DagNode::new("leaf", step(engine_name::PDF, operation::PDF_PAGE_COUNT)),
        ];
        def.edges = vec![DagEdge::new("bad", "mid"), DagEdge::new("mid", "leaf")];

        let report = run_dag(&engine, &def, MINIMAL_PDF).unwrap();
        let status = |id: &str| report.nodes.iter().find(|n| n.id == id).unwrap().status;
        assert_eq!(status("bad"), DagNodeStatus::Failed);
        assert_eq!(status("mid"), DagNodeStatus::Skipped);
        assert_eq!(status("leaf"), DagNodeStatus::Skipped);
    }

    #[test]
    fn run_dag_rejects_invalid_graph_before_running() {
        let engine = PipelineEngine::new();
        let mut def = diamond();
        def.edges.push(DagEdge::new("d", "a")); // closes a cycle
        assert!(matches!(
            run_dag(&engine, &def, MINIMAL_PDF),
            Err(DagError::Cycle { .. })
        ));
    }

    #[test]
    fn run_dag_enforces_max_steps_node_budget() {
        let mut limits = crate::crates::pipeline_engine::PipelineLimits::default();
        limits.max_steps = 2;
        let engine = PipelineEngine::with_limits(
            limits,
            crate::crates::security_engine::ResourceLimits::default(),
        );
        let def = diamond(); // 4 nodes > max_steps 2
        assert!(matches!(
            run_dag(&engine, &def, MINIMAL_PDF),
            Err(DagError::LimitExceeded(_))
        ));
    }
}
