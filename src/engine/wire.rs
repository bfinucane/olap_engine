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
///
/// The catalog stores cubes under a LOWERCASED key (`Catalog::add_cube` ->
/// `cubes.insert(name.to_lowercase(), cube)`) while the exact casing lives in
/// `Cube::name`. Reading `.keys()` therefore returned `["sales"]`, contradicting
/// the casing guarantee above and making a client's round-trip
/// `GET /v1/sales` -> `describe` echo a name it never advertised. Take the name
/// from the VALUE, not the key.
pub fn datasets(catalog: &Catalog) -> DatasetsResponse {
    let mut names: Vec<String> = catalog.cubes.values().map(|c| c.name.clone()).collect();
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
    let truncated = rs.truncated;
    let headers = rs.headers;

    // Which output columns are dimensions vs measures, decided by the plan
    // (groupBy) rather than by guessing from the header text.
    let group_names: Vec<String> = plan.group_by.iter().map(|g| g.name.clone()).collect();
    let mut measure_names: Vec<String> = plan
        .measures
        .iter()
        .map(|m| m.name.clone())
        .filter(|n| !group_names.contains(n))
        .collect();

    let dimension_names: Vec<String> = group_names.clone();

    // `measureLabels`: pill labels for each MEASURE (not each column).
    let mut measure_labels: Vec<String> = plan
        .measures
        .iter()
        .map(|m| {
            let agg = m.aggregation.as_deref().unwrap_or("SUM");
            format!("{}({})", agg.to_uppercase(), m.name)
        })
        .collect();

    // A MEASURE-LESS cube answered with no measures gets an implicit `value`
    // column from the plan layer (see `build_slice_query`). Treat it as the one
    // measure it is — typed numeric, cells parsed as numbers, label `SUM(value)` —
    // so the client does not render real values as a categorical dimension. This
    // matches what it already got when it named `value` explicitly.
    if plan.measures.is_empty() && headers.iter().any(|h| h == "value") {
        measure_names = vec!["value".to_string()];
        measure_labels = vec!["SUM(value)".to_string()];
    }

    let is_measure = |header: &str| measure_names.iter().any(|m| m == header);

    // Attribute columns joined in from another cube. The plan layer writes "" for a
    // key with no attribute row; on the wire that is `null` — the protocol's "no
    // value" — not an empty string, so a client can tell "absent" from "empty".
    let join_attributes: Vec<String> = plan
        .join
        .as_ref()
        .map(|j| j.attributes.clone())
        .unwrap_or_default();
    let is_join_attribute = |header: &str| join_attributes.iter().any(|a| a == header);

    let column_meta: Vec<ColumnMeta> = headers
        .iter()
        .map(|h| {
            if is_measure(h) {
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
                    if is_measure(header) {
                        match cell.parse::<f64>() {
                            Ok(n) => json!(bounded_precision(n)),
                            Err(_) => json!(cell),
                        }
                    } else if is_join_attribute(header) && cell.is_empty() {
                        Value::Null
                    } else {
                        json!(cell)
                    }
                })
                .collect()
        })
        .collect();

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

/// Significant digits kept on the wire.
///
/// f64 carries ~15-17 significant decimals; summing many leaves accumulates noise
/// in the last ones, which surfaced as a grand total of `26763427.430000003`.
/// Rounding to 15 significant digits removes that noise while keeping every digit
/// a double can meaningfully carry. `"{:.14e}"` is one digit before the point plus
/// fourteen after: fifteen in total.
fn bounded_precision(n: f64) -> f64 {
    if !n.is_finite() || n == 0.0 {
        return n;
    }
    format!("{n:.14e}")
        .parse::<f64>()
        .unwrap_or(n)
}

/// §1 `describe`. Column facts are derived from the cube's dimensions; the
/// measure dimension's members become `measure`-suggested columns.
pub fn describe(catalog: &Catalog, dataset: &str) -> Result<CatalogResponse, WireError> {
    let cube = catalog.get_cube(dataset).ok_or_else(|| WireError {
        code: "unknown_dataset",
        message: format!("Dataset '{}' not found.", dataset),
    })?;

    let dim_count = cube.dimensions.len();
    let measure_idx = cube
        .measure_dimension
        .as_ref()
        .and_then(|m| cube.dimension_names.iter().position(|n| n == m));

    // One pass over the stored cells answers three questions at once: the row
    // count, whether the value cells are numeric or textual, and their bounds.
    // `leaf_row_count` used to scan the same data just to count it, so this costs
    // nothing extra. The scan runs for EVERY cube, because a measure-less cube's
    // cells are the values too (see the implicit `value` column below).
    let cells = cube.store.scan_subcube(&vec![None; dim_count]);
    let row_count = cells.len();

    let (mut has_number, mut has_string) = (false, false);
    let (mut num_min, mut num_max) = (f64::INFINITY, f64::NEG_INFINITY);
    for (_, value) in &cells {
        match value {
            CellValue::Numeric(n) => {
                has_number = true;
                num_min = num_min.min(*n);
                num_max = num_max.max(*n);
            }
            CellValue::String(_) => has_string = true,
        }
    }

    // What the measure column ACTUALLY holds. Text wins over numbers: a mixed
    // column must not be advertised as summable. With no cells stored yet there is
    // nothing to inspect, so fall back to the cube's declared kind — an attribute
    // table holds text, a transactional cube holds numbers.
    let measure_is_numeric = if has_string {
        false
    } else if has_number {
        true
    } else {
        cube.is_aggregating
    };

    let mut columns: Vec<CatalogColumn> = Vec::new();

    for (i, dim_name) in cube.dimension_names.iter().enumerate() {
        let dim = cube.dimensions[i].read().unwrap();
        let is_measure = Some(i) == measure_idx;

        if is_measure && measure_is_numeric {
            columns.push(CatalogColumn {
                name: dim_name.clone(),
                data_type: "number".to_string(),
                logical_type: "real".to_string(),
                // Sparse storage: a coordinate may simply have no cell, so a
                // measure value can be absent although every MEMBER exists.
                nullable: true,
                cardinality: dim.len(),
                not_null: false,
                min: if has_number { json!(bounded_precision(num_min)) } else { Value::Null },
                max: if has_number { json!(bounded_precision(num_max)) } else { Value::Null },
                // A HINT only; Plateau decides dimension vs measure itself.
                suggested_kind: "measure".to_string(),
            });
        } else {
            columns.push(CatalogColumn {
                name: dim_name.clone(),
                // A dimension coordinate is always a string — INCLUDING an
                // attribute measure dimension. `RegionAttr.Attribute` holds
                // "Ava Thompson", not a number; reporting it as `measure`/`number`
                // made a client type a text column as numeric and fail to render
                // it. This used to decide the type from the dimension's ROLE
                // (`is_measure`) rather than from its contents.
                data_type: "string".to_string(),
                logical_type: "string".to_string(),
                // Every row that exists has a member here, so a dimension column is
                // never null. (This used to claim `nullable` AND `notNull` at once
                // for every column, which cannot both be true.)
                nullable: false,
                cardinality: dim.len(),
                not_null: true,
                min: Value::Null,
                max: Value::Null,
                suggested_kind: "dimension".to_string(),
            });
        }
    }

    // A MEASURE-LESS cube stores one bare value per coordinate — the shape
    // `.import` produces when the CSV's last column is the value. The query layer
    // addresses that value as the implicit `value` measure (see
    // `build_slice_query`), so the data pane must advertise it too; otherwise a
    // client sees ONLY dimensions and has nothing to chart, which reads as "this
    // dataset has no measures" when in fact its numbers are just unnamed.
    //
    // Type it from the cells, exactly as for a real measure dimension: numbers ->
    // measure, text -> dimension (an attribute table's values are text).
    if measure_idx.is_none() {
        let numeric = if has_string {
            false
        } else if has_number {
            true
        } else {
            cube.is_aggregating
        };

        // Distinct value count for the data pane. The scan has already paid for the
        // cells; this only adds the set.
        let mut distinct: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_, value) in &cells {
            distinct.insert(match value {
                CellValue::Numeric(n) => format!("{n}"),
                CellValue::String(s) => s.clone(),
            });
        }

        columns.push(CatalogColumn {
            name: "value".to_string(),
            data_type: if numeric { "number" } else { "string" }.to_string(),
            logical_type: if numeric { "real" } else { "string" }.to_string(),
            nullable: true,
            cardinality: distinct.len(),
            not_null: false,
            min: if numeric && has_number { json!(bounded_precision(num_min)) } else { Value::Null },
            max: if numeric && has_number { json!(bounded_precision(num_max)) } else { Value::Null },
            suggested_kind: if numeric { "measure" } else { "dimension" }.to_string(),
        });
    }

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

    // A measure dimension's coordinate is a member NAME ('Sales'), not a value; the
    // value is the stored cell. §2's example is `rows: [["East", 100]]` — a
    // dimension and a measure VALUE — so the measure column must carry the cell,
    // otherwise Plateau's profiling sees a categorical column of member names where
    // the data is numeric.
    let measure_idx = cube
        .measure_dimension
        .as_ref()
        .and_then(|m| all_names.iter().position(|n| n == m));

    let cells = cube.store.scan_subcube(&vec![None; dim_count]);
    let total = cells.len();

    let rows: Vec<Vec<Value>> = cells
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(coords, value)| {
            selected
                .iter()
                .map(|col| {
                    let idx = all_names.iter().position(|n| n == col).unwrap();
                    if Some(idx) == measure_idx {
                        match &value {
                            CellValue::Numeric(n) => json!(bounded_precision(*n)),
                            CellValue::String(s) => json!(s),
                        }
                    } else {
                        let id = coords[idx];
                        let dim = cube.dimensions[idx].read().unwrap();
                        json!(dim.get_name(id))
                    }
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
