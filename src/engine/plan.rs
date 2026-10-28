//! Native query-plan entry point — the ENGINE-PROTOCOL.md v1 contract.
//!
//! WHY THIS EXISTS
//!   The engine's historical front door is SQL text (`engine::sql`). A remote
//!   client (Plateau, a Tableau-like tool) does not speak SQL: it owns display
//!   policy and sends a structured PLAN, letting the engine own every physical
//!   decision. This module is that front door.
//!
//! NORMATIVE SOURCE
//!   `ENGINE-PROTOCOL.md` (and, beneath it, the client's `src/core/protocol.ts`).
//!   The JSON field names, enum spellings and response shape in this file are
//!   the wire format, NOT an internal convenience: they must match that document
//!   field-for-field. Where they disagree, the protocol wins.
//!
//! SCOPE (deliberately narrow)
//!   This is the plan -> engine TRANSLATION layer. There is no socket, no HTTP
//!   binding and no JSON text handling here - a `QueryRequest` may be handed to
//!   `execute_plan` directly, in-process, and tested against the same data the
//!   golden SQL fixtures use. The transport (`EngineTransport`) is a later shell.
//!
//! THE PARTS OF THE CONTRACT MOST LIKELY TO BE GOT WRONG
//!   * `filters[].scope` is load-bearing: `dimension` prunes rows BEFORE
//!     grouping; `measure` filters AGGREGATED values afterwards; `top`/`bottom`
//!     rank whole groups. Conflating them makes "Top 10 products by sales"
//!     return the ten highest-VALUED rows instead of the ten highest-TOTALLING
//!     products - a silent, plausible wrong answer.
//!   * `measureLabels` (response) are pill labels like `SUM(Sales)`, indexed by
//!     MEASURE POSITION, not column position. `columns` carries output names
//!     like `Sum of Sales`. These are two different name spaces.
//!
//! DECLARED-BUT-UNIMPLEMENTED (refused with a typed `unsupported_*` code,
//! never approximated - see `Capabilities`):
//!   * LOD expressions. `FIXED` computes over all rows BEFORE dimension
//!     filters, `INCLUDE`/`EXCLUDE` AFTER; that asymmetry is invisible in the
//!     output, so a wrong implementation returns numbers that look reasonable.
//!     We therefore set `supportsLod: false` and let the client compute them.
//!   * measure-scope `top`/`bottom` ranking (and measure-scope filters generally).
//!
//! IMPLEMENTED, so deliberately absent from that list:
//!   * expression evaluation - `engine::calc`, applied in
//!     `apply_calculated_fields`; `capabilities.expressionFunctions` is its list.
//!   * dimension-scope filtering - `in` / `equals` / `notEquals`, the last being
//!     the complement of `equals` over the dimension's own members.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::catalog::catalog::Catalog;
use crate::cube::cube::{Cube, ResultSet, SliceQuery};

/// The plan wire format version this engine understands. Kept in lockstep with
/// the client's `protocolVersion`; a mismatch is `version_mismatch`, never a
/// guess.
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Request shape (ENGINE-PROTOCOL.md §3, mirrors src/core/protocol.ts)
// ---------------------------------------------------------------------------

/// A `query` request. Field names are the wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    /// Which cube/table to query.
    pub dataset: String,
    /// The protocol version the caller is speaking.
    #[serde(rename = "protocolVersion", default = "default_protocol_version")]
    pub protocol_version: u32,
    /// Dimensions to group by, in output order.
    #[serde(rename = "groupBy", default)]
    pub group_by: Vec<GroupBy>,
    /// Measures to compute (pivoted into columns).
    #[serde(default)]
    pub measures: Vec<Measure>,
    /// Row filters. Scope decides whether they run before or after grouping.
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Optional ordering of the result rows.
    #[serde(rename = "orderBy", default)]
    pub order_by: Option<OrderBy>,
    /// Optional cap on the number of returned rows.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Portable expression ASTs, keyed by calculated-field name. A `Measure` may
    /// also carry its own inline AST; both forms are accepted (see `ExprRef`).
    #[serde(default)]
    pub expressions: HashMap<String, AstNode>,
    /// Columns whose distinct value domain is requested. Folded into the
    /// response's `domain` map alongside the rows (see ENGINE-PROTOCOL.md §3).
    #[serde(rename = "domainFor", default)]
    pub domain_for: Vec<String>,
    /// DIMENSION ACCESS (SECURITY-DESIGN.md S3): member names the caller may not see, keyed
    /// by dimension. Filled in by the transport, never by the client - `skip` means it is
    /// absent from the wire format in BOTH directions, so a request cannot ask to be
    /// restricted (harmless) nor move a restriction somewhere else (not harmless).
    #[serde(skip)]
    pub denied_members: HashMap<String, Vec<String>>,

    /// Optional attribute join on a conformed (shared) dimension. Uses the same
    /// routine as the SQL `JOIN ... USING (...)` path — see `engine::attribute_join`.
    #[serde(default)]
    pub join: Option<JoinRequest>,
}

/// A join in the plan: attach attributes from another cube through a CONFORMED
/// dimension — a dimension with the same name in both cubes.
///
/// This is the only kind of join the engine supports: cubes are not relational
/// tables, so there is no arbitrary predicate to join on. The joined cube is an
/// attribute cube whose measure dimension names the attributes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    /// The attribute cube to join onto the queried dataset.
    pub cube: String,
    /// The conformed dimension to key on. Absent means "infer it", which succeeds
    /// only when exactly one dimension is shared; otherwise `invalid_argument`.
    #[serde(default)]
    pub using: Option<String>,
    /// Attribute members to attach, in output order. Each must be a member of the
    /// joined cube's measure dimension. The columns are APPENDED to the response,
    /// after the queried columns; a missing attribute value is JSON `null`.
    #[serde(default)]
    pub attributes: Vec<String>,
}

fn default_protocol_version() -> u32 {
    PROTOCOL_VERSION
}

/// One dimension of the grouping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupBy {
    /// Dimension (or hierarchy) name.
    pub name: String,
    /// Optional date part to bucket by (`year | quarter | month | week | day`).
    /// Reserved: the engine has no temporal bucketing yet, so a non-empty value
    /// is a typed `unsupported_*` error rather than a silent no-op.
    #[serde(rename = "datePart", default)]
    pub date_part: Option<String>,
}

/// A measure to compute.
///
/// The protocol allows either a bare aggregation (`{ name, aggregation }`) or a
/// calculated field (`{ name, expr }`). Following protocol.ts, `expr` is an
/// inline AST; the engine also accepts a bare string, treated as a key into the
/// request's `expressions` map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Measure {
    /// The measure (or calculated-field) name. For a bare aggregation this is
    /// the stored measure member; for a calculated field it is the output name.
    pub name: String,
    /// One of `SUM | AVG | MEDIAN | MIN | MAX | COUNT | COUNTD | STDEV | VAR |
    /// ATTR`. Absent for a calculated field.
    #[serde(default)]
    pub aggregation: Option<String>,
    /// A calculated field: either an inline AST or the name of an entry in the
    /// request's `expressions` map.
    #[serde(default)]
    pub expr: Option<ExprRef>,
}

/// A calculated-field reference: an inline AST, or a name to look up in the
/// request's `expressions` map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ExprRef {
    /// The AST itself (protocol.ts shape).
    Ast(Box<AstNode>),
    /// A key into the request's `expressions` map.
    Named(String),
}

/// The scope of a filter, which decides WHEN it is applied. Load-bearing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FilterScope {
    /// Prunes rows BEFORE grouping (a normal SQL WHERE).
    Dimension,
    /// Filters AGGREGATED values AFTER grouping.
    Measure,
}

/// A single filter predicate. Field names are the wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    /// Client-side identifier. Echoed back if present; not used for logic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The field this filter targets.
    pub field: String,
    /// When this filter runs relative to grouping.
    #[serde(default = "default_filter_scope")]
    pub scope: FilterScope,
    /// One of the 16 operators (see `FilterOp`).
    pub operator: FilterOp,
    /// For `in` / `notIn`.
    #[serde(default)]
    pub values: Vec<String>,
    /// For scalar comparisons (`equals`, `notEquals`, `gt`, `gte`, `lt`, `lte`).
    #[serde(default)]
    pub value: Option<String>,
    /// Lower bound for `between`.
    #[serde(default)]
    pub min: Option<String>,
    /// Upper bound for `between`.
    #[serde(default)]
    pub max: Option<String>,
    /// The N for `top` / `bottom`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// For `top` / `bottom` only.
    #[serde(rename = "includeNulls", default)]
    pub include_nulls: bool,
}

fn default_filter_scope() -> FilterScope {
    FilterScope::Dimension
}

/// The 16 supported filter operators, spelled exactly as the wire format spells
/// them. An unknown operator is a typed error, not silently ignored.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FilterOp {
    #[serde(rename = "in")]
    In,
    #[serde(rename = "notIn")]
    NotIn,
    #[serde(rename = "equals")]
    Equals,
    #[serde(rename = "notEquals")]
    NotEquals,
    #[serde(rename = "gt")]
    Gt,
    #[serde(rename = "gte")]
    Gte,
    #[serde(rename = "lt")]
    Lt,
    #[serde(rename = "lte")]
    Lte,
    #[serde(rename = "between")]
    Between,
    #[serde(rename = "contains")]
    Contains,
    #[serde(rename = "startsWith")]
    StartsWith,
    #[serde(rename = "endsWith")]
    EndsWith,
    #[serde(rename = "isNull")]
    IsNull,
    #[serde(rename = "notNull")]
    NotNull,
    #[serde(rename = "top")]
    Top,
    #[serde(rename = "bottom")]
    Bottom,
}

/// Result ordering. `output` is an OUTPUT column name (`Sum of Sales`), not a
/// measure name (`Sales`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBy {
    /// Output column to sort on.
    pub output: String,
    #[serde(default = "default_direction")]
    pub direction: Direction,
}

fn default_direction() -> Direction {
    Direction::Asc
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Asc,
    Desc,
}

/// The portable expression AST (ENGINE-PROTOCOL.md §3). This is NOT formula
/// text: the client parses Tableau syntax and hands us this fixed, small node
/// set, so the engine never needs a parser for the client's language.
///
/// SERDE REPRESENTATION: externally tagged on a `type` discriminant
/// (`{"type":"field","name":"Sales"}`), matching the TypeScript union in
/// protocol.ts. Note the wire tag is `type`, not `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AstNode {
    #[serde(rename = "number")]
    Number { value: f64 },
    #[serde(rename = "string")]
    String { value: String },
    #[serde(rename = "boolean")]
    Boolean { value: bool },
    #[serde(rename = "null")]
    Null,
    /// A reference to a field (dimension member or measure name).
    #[serde(rename = "field")]
    Field { name: String },
    #[serde(rename = "unary")]
    Unary { op: String, operand: Box<AstNode> },
    #[serde(rename = "binary")]
    Binary { op: String, left: Box<AstNode>, right: Box<AstNode> },
    /// A function call, e.g. `SUM`, `AVG`; `args` are the AST subtrees.
    #[serde(rename = "call")]
    Call { name: String, args: Vec<AstNode> },
    /// `if` with an ordered branch list and an optional `otherwise`, exactly as
    /// protocol.ts defines it (NOT then/else).
    #[serde(rename = "if")]
    If {
        branches: Vec<IfBranch>,
        #[serde(default)]
        otherwise: Option<Box<AstNode>>,
    },
    /// Level-of-detail expression. Reserved: the engine does NOT yet implement
    /// LOD semantics, so any use is `unsupported_expression` (see module docs).
    #[serde(rename = "lod")]
    Lod {
        spec: LodSpec,
        dimensions: Vec<String>,
        expression: Box<AstNode>,
    },
}

/// One `if` branch: a condition and the result when it holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IfBranch {
    pub condition: Box<AstNode>,
    pub result: Box<AstNode>,
}

/// The LOD spec, spelled as protocol.ts spells it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LodSpec {
    #[serde(rename = "FIXED")]
    Fixed,
    #[serde(rename = "INCLUDE")]
    Include,
    #[serde(rename = "EXCLUDE")]
    Exclude,
}

// ---------------------------------------------------------------------------
// Capabilities (ENGINE-PROTOCOL.md §4)
// ---------------------------------------------------------------------------

/// What this engine supports, so a client can DEGRADE CORRECTLY instead of
/// silently returning plausible wrong answers. Field names are the wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    /// The plan protocol version this engine speaks.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    /// Aggregations this engine implements. The UI disables the rest.
    pub aggregations: Vec<String>,
    /// Whether COUNTD is exact. TRUE: COUNTD is computed exactly (one set of base leaf
    /// values per group, see `Aggregation::CountDistinct`), so the UI may label it exact.
    #[serde(rename = "countDistinctExact")]
    pub count_distinct_exact: bool,
    /// Filter operators this engine accepts. Plateau disables the rest rather
    /// than submitting a request that fails.
    #[serde(rename = "filterOperators")]
    pub filter_operators: Vec<String>,
    /// Whether LOD expressions are evaluated server-side. FALSE: Plateau then
    /// computes them client-side. See module docs on why `FIXED` must not be
    /// faked.
    #[serde(rename = "supportsLod")]
    pub supports_lod: bool,
    /// Expression function names this engine implements. Unknown names must be
    /// `unsupported_expression`, not a silent null.
    #[serde(rename = "expressionFunctions")]
    pub expression_functions: Vec<String>,
    /// Whether `domainFor` results can be folded into a query response.
    #[serde(rename = "supportsInlineDomain")]
    pub supports_inline_domain: bool,
    /// The largest row count the engine will return.
    #[serde(rename = "maxRows")]
    pub max_rows: usize,
    /// Whether `QueryRequest.join` is honoured.
    ///
    /// Plateau feature-detects on this before offering attribute joins: an older
    /// engine ignores an unknown `join` field, so a client that did not check would
    /// silently get un-joined data rather than an error.
    #[serde(rename = "supportsJoin")]
    pub supports_join: bool,
    /// Whether requests must carry a bearer token (S1, SECURITY-DESIGN.md §5).
    ///
    /// A client reads this at startup and shows a login screen instead of firing
    /// requests that would all be 401. Additive: an older client ignores the field and
    /// simply receives 401s, which is a clearer failure than silently unauthenticated
    /// data access.
    #[serde(rename = "authRequired")]
    pub auth_required: bool,
    /// Whether `POST /v1/{dataset}/write` is available (WRITE-API-DESIGN.md).
    ///
    /// A client feature-detects on this before offering an editable grid: an older engine
    /// would answer the unknown route with `invalid_argument`, which is a confusing way to
    /// discover that writeback is not supported.
    #[serde(rename = "supportsWrite")]
    pub supports_write: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            // ADVERTISE EXACTLY WHAT `Aggregation` CAN COMPUTE, in both directions:
            // `execute_plan` refuses anything outside this list rather than returning a
            // sum under a different label. The list itself lives in
            // `Aggregation::implemented()` - one source for the advertisement and the
            // refusal message, so they cannot disagree.
            //
            // Semantics (AGGREGATIONS-DESIGN.md §4): empty cells are never counted;
            // non-additive aggregations use the BASE leaf value and ignore rollup
            // weights, so on a ragged hierarchy `AVG != SUM / COUNT` by design. COUNTD
            // follows the same rule: it counts DISTINCT BASE values and ignores weights.
            aggregations: crate::cube::aggregation::Aggregation::implemented(),
            // COUNTD IS implemented and exact: an exact set of base leaf values per
            // group, never an estimate. `true` means "label it exact" in the client.
            count_distinct_exact: true,
            // What the engine actually honours today (dimension scope only).
            filter_operators: ["in", "equals", "notEquals"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            // LOD is NOT implemented; the client must compute it (see docs).
            supports_lod: false,
            // The evaluator EXISTS (`engine::calc`), and this is exactly the set of plain
            // functions it runs - advertise the implementation, never an aspiration.
            expression_functions: crate::engine::calc::FUNCTIONS.iter().map(|f| f.to_string()).collect(),
            // `domainFor` is honoured (folded into the response).
            supports_inline_domain: true,
            max_rows: 100_000,
            // Attribute joins on a conformed dimension are implemented
            // (`engine::attribute_join`, shared with SQL).
            supports_join: true,
            // The library cannot know whether it is served behind auth; the binary
            // overrides this from its own flags.
            auth_required: false,
            supports_write: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors (ENGINE-PROTOCOL.md "Errors")
// ---------------------------------------------------------------------------

/// A typed failure from plan execution, carrying the protocol's `code`.
/// Every reachable "bad plan" case is one of these - never a panic.
///
/// The `invalid_argument` / `unsupported_*` codes are the CLIENT's fault (a bug
/// in Plateau); `engine_error` is the engine's. We never return `engine_error`
/// for a bad request - the client must be able to tell a bug from a fault.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    /// No such dataset.
    UnknownDataset(String),
    /// A filter names an operator this engine does not implement.
    UnsupportedFilter(String),
    /// A request asks for an aggregation this engine does not implement.
    UnsupportedAggregation(String),
    /// A request uses an expression function the engine does not implement, or
    /// an LOD expression.
    UnsupportedExpression(String),
    /// The request is malformed or internally inconsistent (client's fault).
    InvalidArgument(String),
    /// The caller's protocol version does not match ours.
    VersionMismatch { found: u32, supported: u32 },
}

impl PlanError {
    /// The protocol `code` for this error.
    pub fn code(&self) -> &'static str {
        match self {
            PlanError::UnknownDataset(_) => "unknown_dataset",
            PlanError::UnsupportedFilter(_) => "unsupported_filter",
            PlanError::UnsupportedAggregation(_) => "unsupported_aggregation",
            PlanError::UnsupportedExpression(_) => "unsupported_expression",
            PlanError::InvalidArgument(_) => "invalid_argument",
            PlanError::VersionMismatch { .. } => "version_mismatch",
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::UnknownDataset(d) => write!(f, "Dataset '{}' not found.", d),
            PlanError::UnsupportedFilter(m) => write!(f, "Unsupported filter: {}", m),
            PlanError::UnsupportedAggregation(m) => write!(f, "Unsupported aggregation: {}", m),
            PlanError::UnsupportedExpression(m) => write!(f, "Unsupported expression: {}", m),
            PlanError::InvalidArgument(m) => write!(f, "Invalid argument: {}", m),
            PlanError::VersionMismatch { found, supported } => write!(
                f,
                "Protocol version {} is not supported (this engine speaks v{}).",
                found, supported
            ),
        }
    }
}

impl std::error::Error for PlanError {}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Executes a `query` against `catalog`.
///
/// Returns a materialised result (see `PlanResult`). Domain requests
/// (`domainFor`) are returned as their own result variant here; folding them
/// into a single `QueryResponse` alongside the rows is the transport's job.
pub fn execute_plan(catalog: &Catalog, plan: &QueryRequest) -> Result<PlanResult, PlanError> {
    if plan.protocol_version != PROTOCOL_VERSION {
        return Err(PlanError::VersionMismatch {
            found: plan.protocol_version,
            supported: PROTOCOL_VERSION,
        });
    }

    // A pure domain request (no grouping/measures) is a different question.
    if !plan.domain_for.is_empty() && plan.group_by.is_empty() && plan.measures.is_empty() {
        return domain_query(catalog, plan);
    }

    // --- 1. Refuse declared-but-unimplemented features, before doing any work
    //        that could return a plausible-but-wrong answer. ---
    for g in &plan.group_by {
        if g.date_part.is_some() {
            return Err(PlanError::UnsupportedExpression(format!(
                "datePart on '{}' (no temporal bucketing yet)",
                g.name
            )));
        }
    }
    for m in &plan.measures {
        // A calculated field is EVALUATED (`engine::calc`) unless it is an LOD, so check
        // the LOD case first: the refusal must name the unsupported construct precisely.
        if let Some(ExprRef::Ast(node)) = &m.expr
            && contains_lod(node)
        {
            return Err(PlanError::UnsupportedExpression(
                "LOD expression (FIXED/INCLUDE/EXCLUDE); the client must compute it".to_string(),
            ));
        }
        if let Some(ExprRef::Named(name)) = &m.expr
            && let Some(node) = plan.expressions.get(name)
            && contains_lod(node)
        {
            return Err(PlanError::UnsupportedExpression(
                "LOD expression (FIXED/INCLUDE/EXCLUDE); the client must compute it".to_string(),
            ));
        }
        // A CALCULATED FIELD is now EVALUATED (engine::calc) rather than refused. Its
        // shape is validated in `build_slice_query`, where the cube is at hand to check
        // that every referenced field is really a measure of it.
        if m.expr.is_some() {
            if m.aggregation.is_some() {
                return Err(PlanError::InvalidArgument(format!(
                    "measure '{}' has both an aggregation and an expression; \
                     a calculated field aggregates inside the expression",
                    m.name
                )));
            }
            continue;
        }

        // Parse the requested aggregation and REFUSE anything the engine cannot
        // honestly compute. Never substitute SUM for another label: a plausible wrong
        // number is the failure mode ENGINE-PROTOCOL.md exists to prevent. An absent
        // `aggregation` means SUM, matching the client's default.
        let aggregation = m.aggregation.as_deref().unwrap_or("SUM");
        if crate::cube::aggregation::Aggregation::parse(aggregation).is_none() {
            return Err(PlanError::UnsupportedAggregation(format!(
                "'{}' on '{}' (implemented: {}; see capabilities.aggregations)",
                aggregation,
                m.name,
                crate::cube::aggregation::Aggregation::implemented().join(", ")
            )));
        }
    }
    for flt in &plan.filters {
        if matches!(flt.operator, FilterOp::Top | FilterOp::Bottom) {
            return Err(PlanError::UnsupportedFilter(format!(
                "measure-scope {} ranking on '{}'",
                operator_name(flt.operator),
                flt.field
            )));
        }
    }

    // --- 2. Translate to the engine's existing slice query. ---
    let slice = build_slice_query(catalog, plan)?;

    // --- 3. Run it. `query_slice` returns a typed error already. ---
    let cube = catalog
        .get_cube(&plan.dataset)
        .ok_or_else(|| PlanError::UnknownDataset(plan.dataset.clone()))?;
    // --- 3. Run it. ---
    let result = cube
        .query_slice(&slice)
        .map_err(PlanError::InvalidArgument)?;

    // --- 3b. Calculated fields: evaluate the expressions over the aggregate slots the
    //          slice just computed, BEFORE the join so the join appends to the final
    //          column set. ---
    let result = apply_calculated_fields(result, plan, cube)?;

    // --- 3a. Attribute join, BEFORE ordering and limiting: an attribute column is a
    //         real output column, so `orderBy` may reference it. ---
    let result = apply_join(catalog, result, plan)?;

    // --- 4. Post-processing: orderBy and limit. ---
    let result = apply_order_and_limit(result, plan)?;

    Ok(PlanResult::Rows(result))
}

/// Attaches joined attributes to a materialised result, using the same routine as
/// the SQL front end (`engine::attribute_join`).
///
/// The wire layer supplies [`attribute_join::WIRE_MISSING`] as the placeholder for a
/// key with no attribute row, and turns that empty cell into JSON `null` — the
/// protocol's way of saying "no value" (SQL keeps its historical `"-"`).
fn apply_join(
    catalog: &Catalog,
    mut result: ResultSet,
    plan: &QueryRequest,
) -> Result<ResultSet, PlanError> {
    let Some(join) = &plan.join else {
        return Ok(result);
    };

    // A missing joined cube is a dataset problem, not a malformed argument.
    if catalog.get_cube(&join.cube).is_none() {
        return Err(PlanError::UnknownDataset(join.cube.clone()));
    }
    let primary = catalog
        .get_cube(&plan.dataset)
        .ok_or_else(|| PlanError::UnknownDataset(plan.dataset.clone()))?;

    // Attribute columns are APPENDED, so a name that already exists would silently
    // overwrite a real output column. Refuse instead of corrupting the response.
    for attribute in &join.attributes {
        if result.headers.iter().any(|h| h == attribute) {
            return Err(PlanError::InvalidArgument(format!(
                "join attribute '{}' collides with an output column of the same name",
                attribute
            )));
        }
    }

    let resolved = crate::engine::attribute_join::resolve(
        catalog,
        primary,
        &plan.dataset,
        &crate::engine::attribute_join::JoinSpec {
            cube: join.cube.clone(),
            using: join.using.clone(),
        },
    )
    .map_err(PlanError::InvalidArgument)?;

    crate::engine::attribute_join::apply(
        catalog,
        &mut result,
        &resolved,
        &join.attributes,
        crate::engine::attribute_join::WIRE_MISSING,
        // The wire's `join` has no type: it is always LEFT (keep every primary row).
        // INNER exists only in the SQL front end, where a bare JOIN can say so.
        crate::engine::attribute_join::JoinMode::Left,
    )
    .map_err(PlanError::InvalidArgument)?;

    Ok(result)
}

/// The result of executing a plan.
#[derive(Debug, Clone)]
pub enum PlanResult {
    /// A materialised result set.
    Rows(ResultSet),
    /// Distinct values per requested column, in display order.
    Domains(HashMap<String, Vec<String>>),
}

/// Translates the plan's groupBy/measures/filters into a `SliceQuery`.
/// Resolves a calculated-field reference to its AST.
fn resolve_expression<'a>(
    expr_ref: &'a ExprRef,
    plan: &'a QueryRequest,
) -> Result<&'a AstNode, PlanError> {
    match expr_ref {
        ExprRef::Ast(ast) => Ok(ast),
        ExprRef::Named(key) => plan.expressions.get(key).ok_or_else(|| {
            PlanError::InvalidArgument(format!(
                "calculated field references '{}', which is not in the request's \
                 `expressions` map",
                key
            ))
        }),
    }
}

/// A field aggregated inside an expression must be a MEASURE member of the cube.
///
/// Without this, a typo would silently aggregate nothing and yield an absent column —
/// the "confident absence" failure mode, which reads like real data.
fn assert_is_measure(cube: &crate::cube::cube::Cube, field: &str) -> Result<(), PlanError> {
    // A MEASURE-LESS cube has exactly one implicit measure, `value`, which is not a
    // member of any measure dimension and so cannot be checked that way. It is
    // addressable from a plain measure AND from inside an expression — the two paths
    // must agree, or `SUM(value) * 2` fails where `value` alone works.
    if cube.measure_dimension.is_none() && field.eq_ignore_ascii_case("value") {
        return Ok(());
    }
    let Some(measure_dim) = &cube.measure_dimension else {
        return Err(PlanError::InvalidArgument(format!(
            "'{}' cannot be aggregated: cube '{}' has no measure dimension",
            field, cube.name
        )));
    };
    let idx = cube
        .dimension_names
        .iter()
        .position(|n| n == measure_dim)
        .ok_or_else(|| {
            PlanError::InvalidArgument(format!(
                "measure dimension '{}' is missing from cube '{}'",
                measure_dim, cube.name
            ))
        })?;
    let dim = cube.dimensions[idx].read().unwrap();
    if dim.get_id(field).is_none() {
        return Err(PlanError::InvalidArgument(format!(
            "'{}' is not a measure of '{}'",
            field, cube.name
        )));
    }
    Ok(())
}

/// Rewrites a request so an implicit `value` measure becomes the cube's actual measure
/// members (one measure per member, keeping each measure's aggregation).
///
/// Both front doors need this BEFORE the response is shaped. The wire decides whether a
/// column is a measure by looking at the REQUEST, so a `value` that only expanded inside
/// `execute_plan` produced columns the response did not recognise as measures — they came
/// back as the literal string `"-"` instead of `null`, and with no measure labels.
pub fn expand_implicit_value(catalog: &Catalog, plan: &QueryRequest) -> QueryRequest {
    let Some(cube) = catalog.get_cube(&plan.dataset) else {
        return plan.clone();
    };
    // Only a cube WITH a measure dimension has anything to expand into; a measure-less
    // cube's implicit `value` is already the real thing.
    if cube.measure_dimension.is_none()
        || !plan
            .measures
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case("value") && m.expr.is_none())
    {
        return plan.clone();
    }

    let members = cube.measure_member_names();
    let mut expanded = plan.clone();
    let mut measures = Vec::with_capacity(plan.measures.len() + members.len());
    for m in &plan.measures {
        if m.name.eq_ignore_ascii_case("value") && m.expr.is_none() {
            for member in &members {
                let mut measure = m.clone();
                measure.name = member.clone();
                measures.push(measure);
            }
        } else {
            measures.push(m.clone());
        }
    }
    expanded.measures = measures;
    expanded
}

/// Records how a field folds, refusing a SECOND, different aggregation of it.
///
/// v1 allows one aggregation per field (decision 4): the slice result names a column by
/// measure, so `SUM([Sales]) / AVG([Sales])` cannot be represented yet.
fn record_slot(
    slots: &mut HashMap<String, crate::cube::aggregation::Aggregation>,
    field: &str,
    aggregation: crate::cube::aggregation::Aggregation,
) -> Result<(), PlanError> {
    match slots.get(field) {
        Some(existing) if *existing != aggregation => Err(PlanError::InvalidArgument(format!(
            "field '{}' is aggregated twice, as {} and as {}; \
             v1 supports one aggregation per field",
            field,
            existing.name(),
            aggregation.name()
        ))),
        Some(_) => Ok(()),
        None => {
            slots.insert(field.to_string(), aggregation);
            Ok(())
        }
    }
}

/// Evaluates each calculated measure over the slice's per-group aggregate slots.
///
/// Absence propagates (decision 2): a group where an input measure has no cells yields
/// ABSENT, not 0 — `ZN`/`IFNULL` are how a formula asks for 0 instead.
fn apply_calculated_fields(
    result: ResultSet,
    plan: &QueryRequest,
    _cube: &crate::cube::cube::Cube,
) -> Result<ResultSet, PlanError> {
    if !plan.measures.iter().any(|m| m.expr.is_some()) {
        return Ok(result);
    }

    let index: HashMap<&str, usize> = result
        .headers
        .iter()
        .enumerate()
        .map(|(i, h)| (h.as_str(), i))
        .collect();

    enum Column {
        Plain(usize),
        Calc(AstNode, Vec<(String, usize)>),
    }
    let mut columns: Vec<Column> = Vec::with_capacity(plan.measures.len());

    for m in &plan.measures {
        match &m.expr {
            Some(expr_ref) => {
                let ast = resolve_expression(expr_ref, plan)?.clone();
                let mut raw = Vec::new();
                crate::engine::calc::collect_slots(&ast, &mut raw)
                    .map_err(PlanError::UnsupportedExpression)?;
                let mut slots = Vec::with_capacity(raw.len());
                for (field, _) in raw {
                    let idx = *index.get(field.as_str()).ok_or_else(|| {
                        PlanError::InvalidArgument(format!(
                            "calculated field '{}': '{}' is missing from the slice result",
                            m.name, field
                        ))
                    })?;
                    slots.push((field, idx));
                }
                columns.push(Column::Calc(ast, slots));
            }
            None => {
                let idx = *index.get(m.name.as_str()).ok_or_else(|| {
                    PlanError::InvalidArgument(format!(
                        "measure '{}' is missing from the slice result",
                        m.name
                    ))
                })?;
                columns.push(Column::Plain(idx));
            }
        }
    }

    let mut group_idx = Vec::with_capacity(plan.group_by.len());
    for g in &plan.group_by {
        let idx = *index.get(g.name.as_str()).ok_or_else(|| {
            PlanError::InvalidArgument(format!(
                "groupBy column '{}' is missing from the slice result",
                g.name
            ))
        })?;
        group_idx.push(idx);
    }

    let mut headers: Vec<String> = plan.group_by.iter().map(|g| g.name.clone()).collect();
    headers.extend(plan.measures.iter().map(|m| m.name.clone()));

    let mut rows = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut new_row = Vec::with_capacity(headers.len());
        for &i in &group_idx {
            new_row.push(row.get(i).cloned().unwrap_or_default());
        }
        for column in &columns {
            match column {
                Column::Plain(i) => new_row.push(row.get(*i).cloned().unwrap_or_default()),
                Column::Calc(ast, slots) => {
                    let lookup = |field: &str| -> Option<f64> {
                        slots
                            .iter()
                            .find(|(f, _)| f == field)
                            .and_then(|(_, i)| row.get(*i))
                            .and_then(|cell| cell.parse::<f64>().ok())
                    };
                    let value = crate::engine::calc::evaluate(ast, &lookup)
                        .map_err(PlanError::UnsupportedExpression)?;
                    match value.into_number().map_err(PlanError::UnsupportedExpression)? {
                        Some(n) => new_row.push(n.to_string()),
                        // Absent: the engine's marker, which the wire turns into null.
                        None => new_row.push("-".to_string()),
                    }
                }
            }
        }
        rows.push(new_row);
    }

    Ok(ResultSet {
        headers,
        rows,
        truncated: result.truncated,
    })
}

fn build_slice_query(catalog: &Catalog, plan: &QueryRequest) -> Result<SliceQuery, PlanError> {
    let cube = catalog
        .get_cube(&plan.dataset)
        .ok_or_else(|| PlanError::UnknownDataset(plan.dataset.clone()))?;

    let mut axes: Vec<String> = Vec::new();
    let mut axis_hierarchies: HashMap<String, String> = HashMap::new();
    let mut column_dim_index: HashMap<String, usize> = HashMap::new();

    for g in &plan.group_by {
        // A groupBy name may be a dimension OR a hierarchy owned by one. Reuse
        // the same resolution the SQL layer uses so a hierarchy name is a valid
        // axis and displays correctly.
        if let Some(idx) = cube.dimension_names.iter().position(|n| n == &g.name) {
            if !axes.contains(&g.name) {
                axes.push(g.name.clone());
            }
            column_dim_index.insert(g.name.clone(), idx);
        } else if let Some((owner, Some(hier))) = catalog.dimension_of_reference(&g.name)
            && let Some(idx) = cube.dimension_names.iter().position(|n| n == &owner)
        {
            if !axes.contains(&owner) {
                axes.push(owner.clone());
            }
            axis_hierarchies.insert(owner.clone(), hier);
            column_dim_index.insert(g.name.clone(), idx);
        } else {
            return Err(PlanError::InvalidArgument(format!(
                "groupBy names unknown column '{}'",
                g.name
            )));
        }
    }

    // Measures. A bare aggregation names a stored member of the measure dimension;
    // a calculated field contributes its AGGREGATE SLOTS as hidden inputs, and the
    // expression itself is evaluated after the slice returns (`apply_calculated_fields`).
    let mut requested_measures: Vec<String> = Vec::new();
    let mut aggregations: HashMap<String, crate::cube::aggregation::Aggregation> = HashMap::new();
    let mut has_calculated = false;
    // Set when `value` was expanded into the measure members, so `output_columns` comes
    // from the expansion rather than from the requested names.
    let mut expanded_value = false;

    for m in &plan.measures {
        if let Some(expr_ref) = &m.expr {
            has_calculated = true;
            let ast = resolve_expression(expr_ref, plan)?;
            let mut slots = Vec::new();
            crate::engine::calc::collect_slots(ast, &mut slots)
                .map_err(PlanError::UnsupportedExpression)?;
            for (field, aggregation) in slots {
                assert_is_measure(cube, &field)?;
                record_slot(&mut aggregations, &field, aggregation)?;
                if !requested_measures.contains(&field) {
                    requested_measures.push(field);
                }
            }
        } else if m.name.eq_ignore_ascii_case("value") && cube.measure_dimension.is_some() {
            // `value` STANDS FOR ALL MEASURES when the cube defines a measure dimension:
            // one output column per measure member, in display order. More useful than an
            // error, and consistent with the SQL front end.
            //
            // On a cube whose measure dimension is still EMPTY this expands to nothing, so
            // the query returns 0 rows — the honest answer for "no data yet" — instead of
            // an error about a measure that cannot exist yet.
            expanded_value = true;
            let parsed = m
                .aggregation
                .as_deref()
                .and_then(crate::cube::aggregation::Aggregation::parse)
                .unwrap_or(crate::cube::aggregation::Aggregation::Sum);
            for member in cube.measure_member_names() {
                record_slot(&mut aggregations, &member, parsed)?;
                if !requested_measures.contains(&member) {
                    requested_measures.push(member);
                }
            }
        } else {
            // The measure-less `value` exemption lives in `assert_is_measure`, so the
            // plain path and the expression path cannot disagree about it.
            assert_is_measure(cube, &m.name)?;
            let aggregation = m.aggregation.as_deref().unwrap_or("SUM");
            if let Some(parsed) = crate::cube::aggregation::Aggregation::parse(aggregation) {
                record_slot(&mut aggregations, &m.name, parsed)?;
            }
            if !requested_measures.contains(&m.name) {
                requested_measures.push(m.name.clone());
            }
        }
    }

    // Dimension-scope filters become the slice's WHERE. Measure-scope filters
    // are rejected earlier (top/bottom) or, for now, unsupported.
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    for flt in &plan.filters {
        match flt.scope {
            FilterScope::Dimension => {
                // ONE validation for every dimension-scope field, before operator
                // dispatch: the slice's filter map is keyed by dimension name, so a field
                // that is not an exact dimension of this cube could never apply. Dropping
                // it silently would answer a predicate the client believes it sent.
                let dim_index = dimension_index_for_filter(catalog, cube, &flt.field)?;
                let vals = filter_values(cube, dim_index, flt)?;
                filters.entry(flt.field.clone()).or_default().extend(vals);
            }
            FilterScope::Measure => {
                return Err(PlanError::UnsupportedFilter(format!(
                    "measure-scope filter '{} {}' (post-aggregation filtering not implemented yet)",
                    flt.field,
                    operator_name(flt.operator)
                )));
            }
        }
    }

    let mut output_columns: Vec<String> = plan.group_by.iter().map(|g| g.name.clone()).collect();
    if has_calculated || expanded_value {
        // The slice returns the raw aggregate slots; `apply_calculated_fields` evaluates
        // the expressions and renames the columns to the requested names.
        for field in &requested_measures {
            if !output_columns.contains(field) {
                output_columns.push(field.clone());
            }
        }
    } else {
        for m in &plan.measures {
            if !output_columns.contains(&m.name) {
                output_columns.push(m.name.clone());
            }
        }
    }

    // A MEASURE-LESS CUBE has exactly one implicit measure: its stored cell
    // value, addressed as `value`. `sql.rs` already answers
    // `SELECT Region, value FROM ...` that way (`output_columns.push("value")`),
    // but the plan layer only did it when the list was completely empty — so
    // `groupBy: ["Region"]` returned rows of bare member names with no numbers at
    // all, which a client can only read as an empty dataset. Naming `value`
    // explicitly already worked; this makes the two spellings agree.
    //
    // Only for cubes WITHOUT a measure dimension: when one exists, its members are
    // the measures and the client must name them (pivoting needs the choice).
    if plan.measures.is_empty()
        && cube.measure_dimension.is_none()
        && !output_columns.contains(&"value".to_string())
    {
        output_columns.push("value".to_string());
    }

    if output_columns.is_empty() {
        output_columns.push("value".to_string());
    }

    Ok(SliceQuery {
        output_columns,
        axes,
        requested_measures,
        filters,
        axis_hierarchies,
        column_dim_index,
        aggregations,
        denied_leaves: cube.denied_leaf_ids(&plan.denied_members),
    })
}

/// Validates a dimension-scope filter's `field` against the cube's dimensions.
///
/// `SliceQuery.filters` is keyed by DIMENSION name, so a field that is not an exact
/// dimension name of this cube can never apply. Dropping such a predicate silently would
/// answer a filter the client believes it sent - the failure mode the contract forbids
/// (PROTOCOL §5.6 MUST; §11.9 / owner ruling D2). Matching is EXACT and case-sensitive:
/// clients MUST send the name exactly as `describe` returns it.
///
///   * an exact dimension name -> that dimension's index;
///   * a hierarchy name (of a dimension THIS dataset uses) -> `unsupported_filter`,
///     because the slice keys filters by dimension and hierarchy-scoped filtering is not
///     implemented;
///   * anything else -> `invalid_argument`, naming the field and the dataset.
fn dimension_index_for_filter(
    catalog: &Catalog,
    cube: &Cube,
    field: &str,
) -> Result<usize, PlanError> {
    if let Some(index) = cube.dimension_names.iter().position(|name| name == field) {
        return Ok(index);
    }
    // Distinguish "this is a hierarchy, not a dimension" so the client gets the exact
    // code the contract promises. Only a hierarchy of a dimension THIS dataset uses is
    // reported as one; a hierarchy of some other cube is simply not a dimension here.
    if let Some((owner, Some(hierarchy))) = catalog.dimension_of_reference(field)
        && cube.dimension_names.iter().any(|name| name == &owner)
    {
        return Err(PlanError::UnsupportedFilter(format!(
            "filter field '{}' names hierarchy '{}' of dimension '{}' in dataset '{}'; \
             filters are keyed by DIMENSION name, so filter on the dimension itself",
            field, hierarchy, owner, cube.name
        )));
    }
    Err(PlanError::InvalidArgument(format!(
        "filter field '{}' is not a dimension of dataset '{}' (filter fields are matched \
         case-sensitively; send the name exactly as describe returns it)",
        field, cube.name
    )))
}

/// Extracts the engine-facing filter values for a dimension-scope filter.
///
/// The filter field has already been validated by `dimension_index_for_filter` (shared by
/// every dimension-scope operator), so `dim_index` is a real dimension of `cube`.
///
/// The engine's `SliceQuery.filters` is an INCLUSIVE name -> set-of-members map: a
/// dimension is scanned only for the members listed. That expresses `equals` and `in`
/// directly; `notEquals` is expressed as the complement over the dimension's own
/// members - the negation of `equals` (ENGINE-PROTOCOL.md §3 "filters"). Operators this
/// plumbing cannot express are refused with `unsupported_filter` rather than dropped.
fn filter_values(cube: &Cube, dim_index: usize, flt: &Filter) -> Result<Vec<String>, PlanError> {
    match flt.operator {
        FilterOp::Equals => {
            let v = flt.value.clone().ok_or_else(|| {
                PlanError::InvalidArgument(format!(
                    "operator 'equals' on '{}' needs a 'value'",
                    flt.field
                ))
            })?;
            Ok(vec![v])
        }
        FilterOp::In => {
            if flt.values.is_empty() {
                return Err(PlanError::InvalidArgument(format!(
                    "operator 'in' on '{}' needs at least one value",
                    flt.field
                )));
            }
            Ok(flt.values.clone())
        }
        FilterOp::NotEquals => not_equals_values(cube, dim_index, flt),
        other => Err(PlanError::UnsupportedFilter(format!(
            "operator '{}' on '{}'",
            operator_name(other),
            flt.field
        ))),
    }
}

/// Dimension-scope `notEquals`: every member of the dimension EXCEPT the excluded one,
/// as the inclusive member filter the slice understands.
///
/// The field was validated as an exact dimension of this cube by
/// `dimension_index_for_filter` (shared with every other dimension-scope operator), so
/// `dim_index` is valid; there is no second, divergent field check here.
///
/// Only LEAF members are emitted. A consolidation re-admits the leaves beneath it, and
/// a leaf may be reachable from more than one member, so emitting consolidations could
/// re-admit an excluded leaf. Leaves partition the stored cells exactly.
fn not_equals_values(
    cube: &Cube,
    dim_index: usize,
    flt: &Filter,
) -> Result<Vec<String>, PlanError> {
    let excluded = flt.value.clone().ok_or_else(|| {
        PlanError::InvalidArgument(format!(
            "operator 'notEquals' on '{}' needs a 'value'",
            flt.field
        ))
    })?;

    // A filter value may be hierarchy-qualified `Hierarchy:Member`, the same spelling
    // the slice's own filter resolution accepts.
    let (hier, member) = split_qualified_filter_value(&excluded);

    let dim_arc = &cube.dimensions[dim_index];
    let mut dim = dim_arc
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let excluded_leaves: HashSet<u32> = dim
        .get_leaf_descendants_in(hier, member)
        .into_keys()
        .collect();
    if excluded_leaves.is_empty() {
        return Err(PlanError::InvalidArgument(format!(
            "operator 'notEquals' on '{}': member '{}' not found",
            flt.field, excluded
        )));
    }

    let mut all_leaves: HashSet<u32> = HashSet::new();
    let mut kept: Vec<String> = Vec::new();
    for name in dim.member_order_names_in(hier) {
        let leaves = dim.get_leaf_descendants_in(hier, &name);
        // A LEAF stands for exactly one stored coordinate; anything else expands, and is
        // skipped so no excluded leaf can sneak back in through a consolidation.
        let is_leaf = leaves.len() == 1
            && dim
                .get_id_in(hier, &name)
                .is_some_and(|id| leaves.contains_key(&id));
        all_leaves.extend(leaves.keys().copied());
        if is_leaf && !leaves.keys().any(|id| excluded_leaves.contains(id)) {
            kept.push(name);
        }
    }

    // SAFETY NET: if some stored leaf is not addressable through the member list, the
    // complement above cannot be expressed as an inclusive filter. Refuse rather than
    // answer with a list that silently drops rows.
    if all_leaves.len() != dim.leaf_count() {
        return Err(PlanError::UnsupportedFilter(format!(
            "operator 'notEquals' on '{}': the dimension has {} leaf members but only {} \
             are addressable in hierarchy '{}'",
            flt.field,
            dim.leaf_count(),
            all_leaves.len(),
            hier.unwrap_or_else(|| dim.default_hierarchy_name()),
        )));
    }

    // An empty list is meaningful (the excluded member was the only one): it is how the
    // inclusive filter says "keep no rows", which is the correct answer to `notEquals`
    // over a one-member dimension.
    Ok(kept)
}

/// Splits a hierarchy-qualified filter value `Hierarchy:Member` the way the slice's
/// filter resolution does (`cube::split_qualified` is private to the cube module).
fn split_qualified_filter_value(value: &str) -> (Option<&str>, &str) {
    match value.split_once(':') {
        Some((hier, member)) if !hier.is_empty() && !member.is_empty() => (Some(hier), member),
        _ => (None, value),
    }
}

/// True if the AST contains an LOD node anywhere.
fn contains_lod(node: &AstNode) -> bool {
    match node {
        AstNode::Lod { .. } => true,
        AstNode::Unary { operand, .. } => contains_lod(operand),
        AstNode::Binary { left, right, .. } => contains_lod(left) || contains_lod(right),
        AstNode::Call { args, .. } => args.iter().any(contains_lod),
        AstNode::If { branches, otherwise } => {
            branches
                .iter()
                .any(|b| contains_lod(&b.condition) || contains_lod(&b.result))
                || otherwise.as_deref().is_some_and(contains_lod)
        }
        _ => false,
    }
}

/// The wire name of a filter operator, for error messages and `Capabilities`.
fn operator_name(op: FilterOp) -> &'static str {
    match op {
        FilterOp::In => "in",
        FilterOp::NotIn => "notIn",
        FilterOp::Equals => "equals",
        FilterOp::NotEquals => "notEquals",
        FilterOp::Gt => "gt",
        FilterOp::Gte => "gte",
        FilterOp::Lt => "lt",
        FilterOp::Lte => "lte",
        FilterOp::Between => "between",
        FilterOp::Contains => "contains",
        FilterOp::StartsWith => "startsWith",
        FilterOp::EndsWith => "endsWith",
        FilterOp::IsNull => "isNull",
        FilterOp::NotNull => "notNull",
        FilterOp::Top => "top",
        FilterOp::Bottom => "bottom",
    }
}

/// Applies `orderBy` and `limit` to a materialised result set. `orderBy.output`
/// is an OUTPUT column name; an unknown name is `invalid_argument`, never a
/// silent sort by something else.
fn apply_order_and_limit(
    result: ResultSet,
    plan: &QueryRequest,
) -> Result<ResultSet, PlanError> {
    let mut result = result;

    if let Some(ob) = &plan.order_by {
        let col = result
            .headers
            .iter()
            .position(|h| h == &ob.output)
            .ok_or_else(|| {
                PlanError::InvalidArgument(format!(
                    "orderBy names unknown output column '{}'",
                    ob.output
                ))
            })?;

        result.rows.sort_by(|a, b| {
            let va = a.get(col).map(String::as_str).unwrap_or("");
            let vb = b.get(col).map(String::as_str).unwrap_or("");
            // Numeric when both parse, else lexicographic. (A richer typed
            // result set is a later step; this keeps the prototype honest.)
            let ord = match (va.parse::<f64>(), vb.parse::<f64>()) {
                (Ok(na), Ok(nb)) => na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal),
                _ => va.cmp(vb),
            };
            match ob.direction {
                Direction::Asc => ord,
                Direction::Desc => ord.reverse(),
            }
        });
    }

    if let Some(limit) = plan.limit {
        // `truncated` means "the limit CUT the result", so it is only true when
        // there were rows to remove. Deriving it later from `rows.len() == limit`
        // reported a cut whenever the data happened to end exactly at the limit,
        // telling a pager there was another page that does not exist.
        if result.rows.len() > limit {
            result.rows.truncate(limit);
            result.truncated = true;
        }
    }

    Ok(result)
}

/// The distinct member values of each `domainFor` column, in display order.
///
/// Shared by the pure-domain path (`domain_query`) and the row-returning query path:
/// ENGINE-PROTOCOL.md §3 says these lists come back **with** the result, so a query
/// that also groups must answer them too. Both forms read this one function, so their
/// lists cannot drift apart.
///
/// DIMENSION ACCESS: a denied member must not leak through a picker list (it does not
/// leak through rows — `query_slice` drops its cells). A member is omitted when ANY of
/// its leaves is denied, which also hides the member's subtree, exactly as denying a
/// consolidation hides its children on every other path.
pub fn domain_map(
    catalog: &Catalog,
    plan: &QueryRequest,
) -> Result<HashMap<String, Vec<String>>, PlanError> {
    let cube = catalog
        .get_cube(&plan.dataset)
        .ok_or_else(|| PlanError::UnknownDataset(plan.dataset.clone()))?;
    let denied = cube.denied_leaf_ids(&plan.denied_members);

    let mut domains: HashMap<String, Vec<String>> = HashMap::new();

    for col in &plan.domain_for {
        // Resolve to (dimension index, optional hierarchy name).
        let resolved: Option<(usize, Option<String>)> =
            if let Some(idx) = cube.dimension_names.iter().position(|n| n == col) {
                Some((idx, None))
            } else if let Some((owner, hier)) = catalog.dimension_of_reference(col) {
                cube.dimension_names
                    .iter()
                    .position(|n| n == &owner)
                    .map(|idx| (idx, hier))
            } else {
                None
            };

        let (idx, hier) = resolved.ok_or_else(|| {
            PlanError::InvalidArgument(format!("domainFor names unknown column '{}'", col))
        })?;
        // `member_order_names_in` ensures (and may rebuild) the display order,
        // so it needs the write lock - exactly as the `.order` REPL command does.
        let mut dim = cube.dimensions[idx].write().unwrap();
        let hier_ref = hier.as_deref();
        let mut names = dim.member_order_names_in(hier_ref);
        if !denied[idx].is_empty() {
            let denied_here = &denied[idx];
            names.retain(|name| {
                dim.get_leaf_descendants_in(hier_ref, name)
                    .keys()
                    .all(|leaf| !denied_here.contains(leaf))
            });
        }
        domains.insert(col.clone(), names);
    }

    Ok(domains)
}

/// Answers a pure domain query: the distinct member values of each requested
/// column, in display order. This is what a filter-picker UI asks for.
fn domain_query(catalog: &Catalog, plan: &QueryRequest) -> Result<PlanResult, PlanError> {
    Ok(PlanResult::Domains(domain_map(catalog, plan)?))
}

/// Renders a result set (or domain answer) as the engine's ASCII table, reusing
/// the same formatting the SQL path uses, so both front doors look identical.
pub fn format_plan_result(result: &PlanResult) -> String {
    match result {
        PlanResult::Rows(rs) => {
            if rs.rows.is_empty() {
                return "0 rows returned.".to_string();
            }
            let mut out = String::new();
            out.push_str(&rs.headers.join(" | "));
            out.push('\n');
            out.push_str(&"-".repeat(rs.headers.len() * 10));
            out.push('\n');
            for row in &rs.rows {
                out.push_str(&row.join(" | "));
                out.push('\n');
            }
            out
        }
        PlanResult::Domains(domains) => {
            let mut out = String::new();
            let mut cols: Vec<&String> = domains.keys().collect();
            cols.sort(); // deterministic output
            for col in cols {
                let vals = &domains[col];
                out.push_str(&format!("{} ({} values): {}\n", col, vals.len(), vals.join(", ")));
            }
            if out.is_empty() {
                out.push_str("0 columns requested.");
            }
            out
        }
    }
}

#[cfg(test)]
mod calculated_field_tests {
    //! End-to-end through `execute_plan`: an AST in, per-group numbers out.

    use super::*;
    use crate::catalog::catalog::Catalog;
    use crate::cube::node::CellValue;

    fn catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog.add_cube("Sales", &["Region", "M"], Some("M"), true);
        {
            let cube = catalog.get_cube_mut("Sales").expect("Sales");
            cube.write(&["East", "Sales"], CellValue::Numeric(100.0)).expect("write");
            cube.write(&["East", "Cost"], CellValue::Numeric(40.0)).expect("write");
            cube.write(&["West", "Sales"], CellValue::Numeric(200.0)).expect("write");
            // West deliberately has NO Cost cell: the absent case.
        }
        catalog
    }

    fn agg(name: &str, field: &str) -> serde_json::Value {
        serde_json::json!({"type": "call", "name": name, "args": [{"type": "field", "name": field}]})
    }

    fn plan(expr: serde_json::Value) -> QueryRequest {
        serde_json::from_value(serde_json::json!({
            "dataset": "Sales",
            "protocolVersion": PROTOCOL_VERSION,
            "groupBy": [{"name": "Region"}],
            "measures": [{"name": "Margin", "expr": expr}]
        }))
        .expect("plan")
    }

    #[test]
    fn a_calculated_measure_is_computed_per_group() {
        let catalog = catalog();
        // Sales - Cost, per region.
        let expr = serde_json::json!({
            "type": "binary", "op": "-", "left": agg("SUM", "Sales"), "right": agg("SUM", "Cost")
        });
        let result = execute_plan(&catalog, &plan(expr)).expect("plan runs");
        let PlanResult::Rows(rows) = result else { panic!("expected rows") };

        assert_eq!(rows.headers, vec!["Region", "Margin"]);
        assert_eq!(rows.rows[0], vec!["East", "60"]);
        // West has no Cost, so the subtraction is ABSENT, not 200 (decision 2).
        assert_eq!(rows.rows[1], vec!["West", "-"]);
    }

    #[test]
    fn zn_is_how_a_formula_asks_for_zero() {
        let catalog = catalog();
        let expr = serde_json::json!({
            "type": "call", "name": "ZN", "args": [{
                "type": "binary", "op": "-", "left": agg("SUM", "Sales"), "right": agg("SUM", "Cost")
            }]
        });
        let result = execute_plan(&catalog, &plan(expr)).expect("plan runs");
        let PlanResult::Rows(rows) = result else { panic!("expected rows") };
        assert_eq!(rows.rows[0], vec!["East", "60"]);
        // West: Sales 200 minus an ABSENT Cost is itself absent (decision 2), so ZN
        // yields 0. The 200 is not subtracted from nothing — there is no subtraction to do.
        assert_eq!(rows.rows[1], vec!["West", "0"], "ZN(absent) is 0");
    }

    #[test]
    fn division_by_zero_is_absent_not_zero() {
        let catalog = catalog();
        let expr = serde_json::json!({
            "type": "binary", "op": "/", "left": agg("SUM", "Cost"), "right": agg("SUM", "Cost")
        });
        let result = execute_plan(&catalog, &plan(expr)).expect("plan runs");
        let PlanResult::Rows(rows) = result else { panic!("expected rows") };
        assert_eq!(rows.rows[0], vec!["East", "1"], "40/40");
        assert_eq!(rows.rows[1], vec!["West", "-"], "no Cost at all: absent, not 0/0");
    }

    #[test]
    fn an_unsupported_function_is_refused_by_name() {
        let catalog = catalog();
        let expr = serde_json::json!({
            "type": "call", "name": "ROUND", "args": [agg("SUM", "Sales"), {"type": "number", "value": 0.0}]
        });
        let err = execute_plan(&catalog, &plan(expr)).expect_err("ROUND is not implemented");
        assert_eq!(err.code(), "unsupported_expression");
        assert!(err.to_string().contains("ROUND"), "{err}");
    }

    #[test]
    fn a_non_measure_field_is_refused() {
        let catalog = catalog();
        let err = execute_plan(&catalog, &plan(agg("SUM", "Region")))
            .expect_err("Region is a dimension, not a measure");
        assert_eq!(err.code(), "invalid_argument");
        assert!(err.to_string().contains("not a measure"), "{err}");
    }

    #[test]
    fn two_aggregations_of_one_field_are_refused() {
        let catalog = catalog();
        let expr = serde_json::json!({
            "type": "binary", "op": "/", "left": agg("SUM", "Sales"), "right": agg("AVG", "Sales")
        });
        let err = execute_plan(&catalog, &plan(expr)).expect_err("mixed aggregation");
        // `unsupported_expression`, not `invalid_argument`: the request is well formed;
        // the engine simply cannot evaluate two aggregations of one field yet.
        assert_eq!(err.code(), "unsupported_expression");
        assert!(err.to_string().contains("one aggregation per field"), "{err}");
    }
}
#[cfg(test)]
mod measure_less_calc_tests {
    //! A measure-less cube addresses its only measure as `value`. The plain path
    //! exempted it; the expression path must too, or `SUM(value) * 2` fails where
    //! `value` alone works (found by probing the seeded catalogue).

    use super::*;
    use crate::catalog::catalog::Catalog;
    use crate::cube::node::CellValue;

    #[test]
    fn a_calculated_field_works_on_a_measure_less_cube() {
        let mut catalog = Catalog::new();
        catalog.add_cube("Flat", &["Region"], None, true);
        {
            let cube = catalog.get_cube_mut("Flat").expect("Flat");
            cube.write(&["East"], CellValue::Numeric(10.0)).expect("write");
            cube.write(&["West"], CellValue::Numeric(30.0)).expect("write");
        }

        let plan: QueryRequest = serde_json::from_value(serde_json::json!({
            "dataset": "Flat",
            "protocolVersion": PROTOCOL_VERSION,
            "groupBy": [{"name": "Region"}],
            "measures": [{
                "name": "Double",
                "expr": {
                    "type": "binary", "op": "*",
                    "left": {"type": "call", "name": "SUM", "args": [{"type": "field", "name": "value"}]},
                    "right": {"type": "number", "value": 2}
                }
            }]
        }))
        .expect("plan");

        let PlanResult::Rows(rows) = execute_plan(&catalog, &plan).expect("runs") else {
            panic!("expected rows")
        };
        assert_eq!(rows.headers, vec!["Region", "Double"]);
        assert_eq!(rows.rows[0], vec!["East", "20"]);
        assert_eq!(rows.rows[1], vec!["West", "60"]);
    }
}