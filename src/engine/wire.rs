//! The HTTP-facing wire layer: the four protocol operations as pure functions.
//!
//! WHY THIS EXISTS
//!   `engine::plan` owns the *plan -> engine* translation and the request shape.
//!   This module owns the *engine -> client* RESPONSE shape (ENGINE-PROTOCOL.md
//!   §1-§4) and the four operations the reference client calls:
//!     * `capabilities` GET /v1/capabilities
//!     * `describe`     GET /v1/{dataset}
//!     * `sample`       GET /v1/{dataset}/sample
//!     * `query`        POST /v1/{dataset}/query
//!
//!   There is still no socket here. These are plain functions that take a
//!   parsed request and return a `serde_json::Value`-ready struct; the binary
//!   (`src/bin/serve.rs`) is the thin shell that reads HTTP and calls them.
//!
//! THE ONE HARD PART
//!   `measureLabels` in a `QueryResponse` carries pill labels (`SUM(Sales)`)
//!   and is indexed by MEASURE POSITION, NOT column position. `columns` carries
//!   output names (`Sum of Sales`). With any dimension present the two indices
//!   differ by the dimension count. Getting this wrong was a real silent-wrong-
//!   chart bug in Plateau, so `build_query_response` constructs the label list
//!   from the measure list directly, never by slicing `columns`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::catalog::catalog::Catalog;
use crate::cube::node::CellValue;
use crate::engine::plan::{
    execute_plan, Capabilities, PlanError, PlanResult, QueryRequest, PROTOCOL_VERSION,
};

// ---------------------------------------------------------------------------
// Response shapes (ENGINE-PROTOCOL.md §1-§4)
// ---------------------------------------------------------------------------

/// §3 `QueryResponse`. Field names are the wire format.
#[derive(Debug, Clone, Serialize)]
pub struct QueryResponse {
    /// Output column names, in output order (`Region`, `Sum of Sales`).
    pub columns: Vec<String>,
    /// Per-column metadata (kind, types, aggregation hint).
    #[serde(rename = "columnMeta")]
    pub column_meta: Vec<ColumnMeta>,
    /// Row-major cells. Cells are JSON scalars or `null`.
    pub rows: Vec<Vec<Value>>,
    /// Output names of the dimension columns, in output order.
    #[serde(rename = "dimensionNames")]
    pub dimension_names: Vec<String>,
    /// Output names of the measure columns, in output order.
    #[serde(rename = "measureNames")]
    pub measure_names: Vec<String>,
    /// Pill labels (`SUM(Sales)`), indexed by MEASURE POSITION. See module docs.
    #[serde(rename = "measureLabels")]
    pub measure_labels: Vec<String>,
    /// Distinct value domains for any `domainFor` columns.
    #[serde(default)]
    pub domain: HashMap<String, Vec<String>>,
    /// Wall-clock duration of the operation, in milliseconds.
    #[serde(rename = "elapsedMs")]
    pub elapsed_ms: u64,
    /// Whether `limit` cut the result.
    pub truncated: bool,
}

/// §3 `columnMeta[]`.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnMeta {
    pub name: String,
    /// `dimension` | `measure`.
    pub kind: String,
    #[serde(rename = "dataType")]
    pub data_type: String,
    #[serde(rename = "logicalType")]
    pub logical_type: String,
    #[serde(rename = "semanticType")]
    pub semantic_type: String,
    #[serde(rename = "defaultAggregation")]
    pub default_aggregation: String,
}

/// §1 `CatalogResponse` (the `describe` answer).
#[derive(Debug, Clone, Serialize)]
pub struct CatalogResponse {
    pub dataset: String,
    #[serde(rename = "rowCount")]
    pub row_count: usize,
    #[serde(rename = "primaryKey")]
    pub primary_key: Value,
    pub columns: Vec<CatalogColumn>,
}

/// §1 `CatalogResponse.columns[]`.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogColumn {
    pub name: String,
    #[serde(rename = "dataType")]
    pub data_type: String,
    #[serde(rename = "logicalType")]
    pub logical_type: String,
    pub nullable: bool,
    pub cardinality: usize,
    #[serde(rename = "notNull")]
    pub not_null: bool,
    pub min: Value,
    pub max: Value,
    #[serde(rename = "suggestedKind")]
    pub suggested_kind: String,
}

/// §2 `SampleResponse`.
#[derive(Debug, Clone, Serialize)]
pub struct SampleResponse {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    #[serde(rename = "rowCount")]
    pub row_count: usize,
}

/// §2 the `sample` query parameters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SampleQuery {
    /// Required by the protocol but defaulted here so a missing `limit` is a
    /// client bug the transport can still answer with `invalid_argument`.
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    /// Optional subset of columns (comma-joined by the transport).
    #[serde(default)]
    pub columns: Vec<String>,
}

/// The error envelope (§ "Errors"): `{ "error": "...", "code": "..." }`.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: String,
}

impl From<&PlanError> for ErrorResponse {
    fn from(e: &PlanError) -> Self {
        Self {
            error: e.to_string(),
            code: e.code().to_string(),
        }
    }
}

/// A wire-level error: a protocol `code` plus a human message. `engine_error` is
/// reserved for genuine engine faults and is never used for a bad request.
#[derive(Debug, Clone)]
pub struct WireError {
    pub code: &'static str,
    pub message: String,
}

impl WireError {
    /// The HTTP status code the reference client expects for each `code`.
    pub fn status(&self) -> u16 {
        match self.code {
            // A bug in the client.
            "invalid_argument" | "unsupported_aggregation" | "unsupported_filter"
            | "unsupported_expression" | "version_mismatch" => 400,
            "unknown_dataset" => 404,
            // The engine's fault.
            _ => 500,
        }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl From<PlanError> for WireError {
    fn from(e: PlanError) -> Self {
        Self {
            code: e.code(),
            message: e.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// §4 `capabilities`. A pure self-declaration; never touches the catalog.
pub fn capabilities() -> Capabilities {
    Capabilities::default()
}

/// `datasets` — the list of dataset names a client may query.
///
/// Plateau cannot guess dataset names, so it discovers them here and then uses
/// each name verbatim in `/v1/{dataset}`. The names are therefore returned in
/// their EXACT stored casing (a caller must be able to round-trip `Sales`, not
/// a case-folded `sales`), sorted for a deterministic response.
#[derive(Debug, Clone, Serialize)]
pub struct DatasetsResponse {
    pub datasets: Vec<String>,
}

/// Builds the dataset list from the catalog's cube names.
pub fn datasets(catalog: &Catalog) -> DatasetsResponse {
    let mut names: Vec<String> = catalog.cubes.keys().cloned().collect();
    names.sort();
    DatasetsResponse { datasets: names }
}

/// §3 `query`. Runs the plan, then folds the result into a full `QueryResponse`.
///
/// `elapsed_ms` is measured here (the whole engine-side duration); `truncated`
/// is derived from whether `limit` actually removed rows.
pub fn query(catalog: &mut Catalog, plan: &QueryRequest) -> Result<QueryResponse, WireError> {
    let started = std::time::Instant::now();

    // How many rows existed BEFORE the limit, so we can report `truncated`
    // honestly. We recompute this only when a limit is present.
    let result = execute_plan(catalog, plan)?;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match result {
        PlanResult::Rows(rs) => Ok(build_query_response(rs, plan, elapsed_ms)),
        PlanResult::Domains(domains) => {
            // A pure domain request: no rows, just the domain map. The shape is
            // still a `QueryResponse`, so the client can parse it uniformly.
            Ok(QueryResponse {
                columns: Vec::new(),
                column_meta: Vec::new(),
                rows: Vec::new(),
                dimension_names: Vec::new(),
                measure_names: Vec::new(),
                measure_labels: Vec::new(),
                domain: domains,
                elapsed_ms,
                truncated: false,
            })
        }
    }
}

/// Assembles the full `QueryResponse` from a materialised result set.
///
/// The two name spaces are kept strictly separate:
///   * `columns` / `dimensionNames` / `measureNames` -> OUTPUT names.
///   * `measureLabels` -> pill labels, indexed by MEASURE POSITION.
fn build_query_response(
    rs: crate::cube::cube::ResultSet,
    plan: &QueryRequest,
    elapsed_ms: u64,
) -> QueryResponse {
    let headers = rs.headers;

    // Which output columns are dimensions vs measures, decided by the plan
    // (groupBy) rather than by guessing from the header text.
    let group_names: Vec<String> = plan.group_by.iter().map(|g| g.name.clone()).collect();
    let measure_names: Vec<String> = plan
        .measures
        .iter()
        .map(|m| m.name.clone())
        .filter(|n| !group_names.contains(n))
        .collect();

    let dimension_names: Vec<String> = group_names.clone();

    // `measureLabels`: pill labels for each MEASURE (not each column).
    let measure_labels: Vec<String> = plan
        .measures
        .iter()
        .map(|m| {
            let agg = m.aggregation.as_deref().unwrap_or("SUM");
            format!("{}({})", agg.to_uppercase(), m.name)
        })
        .collect();

    let column_meta: Vec<ColumnMeta> = headers
        .iter()
        .map(|h| {
            if measure_names.contains(h) {
                ColumnMeta {
                    name: h.clone(),
                    kind: "measure".to_string(),
                    data_type: "number".to_string(),
                    logical_type: "real".to_string(),
                    semantic_type: "quantitative".to_string(),
                    default_aggregation: "SUM".to_string(),
                }
            } else {
                ColumnMeta {
                    name: h.clone(),
                    kind: "dimension".to_string(),
                    data_type: "string".to_string(),
                    logical_type: "string".to_string(),
                    semantic_type: "nominal".to_string(),
                    default_aggregation: "COUNT".to_string(),
                }
            }
        })
        .collect();

    // Cells become JSON scalars: measures parse to numbers, dimensions stay
    // strings. An unparseable numeric cell falls back to its string form rather
    // than becoming `null` (which the client would read as "missing").
    let rows: Vec<Vec<Value>> = rs
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .zip(headers.iter())
                .map(|(cell, header)| {
                    if measure_names.contains(header) {
                        match cell.parse::<f64>() {
                            Ok(n) => json!(n),
                            Err(_) => json!(cell),
                        }
                    } else {
                        json!(cell)
                    }
                })
                .collect()
        })
        .collect();

    // `truncated`: true when a limit was requested AND the result filled it
    // exactly. That is the only case an already-truncated result can report
    // without the pre-limit row count, which `execute_plan` has already
    // discarded. A result with FEWER rows than the limit provably was not cut;
    // a result that equals the limit may have been (and, for a client sizing a
    // pager, assuming it may have been is the safe reading).
    let truncated = matches!(plan.limit, Some(l) if rows.len() == l);

    QueryResponse {
        columns: headers,
        column_meta,
        rows,
        dimension_names,
        measure_names,
        measure_labels,
        domain: HashMap::new(),
        elapsed_ms,
        truncated,
    }
}

/// §1 `describe`. Column facts are derived from the cube's dimensions; the
/// measure dimension's members become `measure`-suggested columns.
pub fn describe(catalog: &Catalog, dataset: &str) -> Result<CatalogResponse, WireError> {
    let cube = catalog.get_cube(dataset).ok_or_else(|| WireError {
        code: "unknown_dataset",
        message: format!("Dataset '{}' not found.", dataset),
    })?;

    let mut columns: Vec<CatalogColumn> = Vec::new();

    for (i, dim_name) in cube.dimension_names.iter().enumerate() {
        let dim = cube.dimensions[i].read().unwrap();
        let is_measure = cube.measure_dimension.as_deref() == Some(dim_name.as_str());
        columns.push(CatalogColumn {
            name: dim_name.clone(),
            data_type: if is_measure { "number" } else { "string" }.to_string(),
            logical_type: if is_measure { "real" } else { "string" }.to_string(),
            nullable: true,
            cardinality: dim.len(),
            not_null: true,
            min: Value::Null,
            max: Value::Null,
            // A HINT only; Plateau decides dimension vs measure itself.
            suggested_kind: if is_measure { "measure" } else { "dimension" }.to_string(),
        });
    }

    let row_count = leaf_row_count(cube);

    Ok(CatalogResponse {
        dataset: dataset.to_string(),
        row_count,
        primary_key: Value::Null,
        columns,
    })
}

/// §2 `sample`. Returns up to `limit` leaf rows, skipping `offset`.
pub fn sample(
    catalog: &Catalog,
    dataset: &str,
    params: &SampleQuery,
) -> Result<SampleResponse, WireError> {
    let cube = catalog.get_cube(dataset).ok_or_else(|| WireError {
        code: "unknown_dataset",
        message: format!("Dataset '{}' not found.", dataset),
    })?;

    let limit = params.limit.ok_or_else(|| WireError {
        code: "invalid_argument",
        message: "sample requires a 'limit' parameter.".to_string(),
    })?;
    let offset = params.offset.unwrap_or(0);

    // Column selection: the requested subset (resolved case-insensitively to the
    // cube's casing), else every dimension.
    let all_names: Vec<String> = cube.dimension_names.clone();
    let selected: Vec<String> = if params.columns.is_empty() {
        all_names.clone()
    } else {
        let mut chosen = Vec::new();
        for want in &params.columns {
            match all_names.iter().find(|n| n.eq_ignore_ascii_case(want)) {
                Some(n) => chosen.push(n.clone()),
                None => {
                    return Err(WireError {
                        code: "invalid_argument",
                        message: format!("sample names unknown column '{}'.", want),
                    });
                }
            }
        }
        chosen
    };

    // Walk every leaf cell and project the selected columns.
    let dim_count = cube.dimensions.len();
    let cells = cube.store.scan_subcube(&vec![None; dim_count]);
    let total = cells.len();

    let rows: Vec<Vec<Value>> = cells
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(coords, _value)| {
            selected
                .iter()
                .map(|col| {
                    let idx = all_names.iter().position(|n| n == col).unwrap();
                    let id = coords[idx];
                    let dim = cube.dimensions[idx].read().unwrap();
                    json!(dim.get_name(id))
                })
                .collect()
        })
        .collect();

    Ok(SampleResponse {
        columns: selected,
        rows,
        row_count: total,
    })
}

/// Counts the leaf cells in a cube (one cell per stored fact row).
fn leaf_row_count(cube: &crate::cube::cube::Cube) -> usize {
    let dim_count = cube.dimensions.len();
    cube.store.scan_subcube(&vec![None; dim_count]).len()
}

// ---------------------------------------------------------------------------
// Helpers shared with the transport
// ---------------------------------------------------------------------------

/// The protocol version this build speaks.
pub fn protocol_version() -> u32 {
    PROTOCOL_VERSION
}

/// Renders a `QueryResponse` as the engine's ASCII table (used by the REPL's
/// `.query` and by debugging; the HTTP binding sends JSON).
pub fn format_query_response(resp: &QueryResponse) -> String {
    if resp.rows.is_empty() {
        return "0 rows returned.".to_string();
    }
    let mut out = resp.columns.join(" | ");
    out.push('\n');
    out.push_str(&"-".repeat(resp.columns.len() * 10));
    out.push('\n');
    for row in &resp.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .collect();
        out.push_str(&cells.join(" | "));
        out.push('\n');
    }
    out
}

/// Convenience: stringify a `CellValue` the same way the engine core does.
pub fn cell_to_value(v: &CellValue) -> Value {
    match v {
        CellValue::Numeric(n) => json!(n),
        CellValue::String(s) => json!(s),
    }
}
