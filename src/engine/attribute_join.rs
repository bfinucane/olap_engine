//! Attribute joins on conformed (shared) dimensions — ONE implementation, used by
//! both front doors.
//!
//! WHY THIS MODULE EXISTS
//!   The SQL path had this logic inline inside a 640-line `execute_sql`, and the
//!   native plan layer had none of it, so a Plateau client could not join at all.
//!   The same routine now serves both: SQL keeps only the parsing, the plan layer
//!   keeps only the JSON, and the join itself cannot behave differently between the
//!   two front doors.
//!
//! WHAT A JOIN IS HERE (deliberately narrow)
//!   Cubes are not relational tables. The ONLY thing two cubes can be joined on is
//!   a CONFORMED DIMENSION: a dimension with the same name in both cubes, holding
//!   the same members. The joined cube is an ATTRIBUTE cube — its measure dimension
//!   names the attributes and its cells hold the values.
//!
//!   `resolve` validates the key: the joined cube exists; `using` (when given)
//!   names a conformed dimension; when omitted, exactly one dimension must
//!   conform, otherwise the request is ambiguous.
//!
//!   `apply` does ONE bulk query on the attribute cube, indexes the result by key,
//!   and stitches the requested attributes into the primary rows.
//!
//! V1 IS A LEFT JOIN
//!   Every primary row survives; attributes appear where the key matches and are
//!   filled with a caller-chosen token where it does not. `RIGHT`/`FULL`/`CROSS`
//!   are refused by the SQL front end rather than approximated, and a second join is
//!   refused rather than silently dropped.

use std::collections::HashMap;

use crate::catalog::catalog::Catalog;
use crate::cube::cube::{Cube, ResultSet};
use crate::cube::node::CellValue;

/// A join as the caller expressed it: which cube, and which dimension to key on.
/// `using: None` asks the engine to infer the single conformed dimension.
#[derive(Debug, Clone)]
pub struct JoinSpec {
    pub cube: String,
    pub using: Option<String>,
}

/// A validated join: the attribute cube and the conformed dimension it keys on.
/// Carries the primary cube's name only so error messages can name both sides.
#[derive(Debug, Clone)]
pub struct ResolvedJoin {
    pub primary: String,
    pub cube: String,
    pub key: String,
}

/// Fills every cell of `key` with `missing`.
///
/// SQL passes `"-"` (its golden transcripts assert it); the plan layer passes `""`
/// because the wire layer turns an empty attribute cell into JSON `null`.
pub const SQL_MISSING: &str = "-";
pub const WIRE_MISSING: &str = "";

/// Validates a join and resolves its key dimension.
///
/// The error strings are the ones the `join.sql` golden has always asserted; they
/// are user-facing guidance, not internal detail, so they live here unchanged.
pub fn resolve(
    catalog: &Catalog,
    primary: &Cube,
    primary_name: &str,
    spec: &JoinSpec,
) -> Result<ResolvedJoin, String> {
    let joined = catalog
        .get_cube(&spec.cube)
        .ok_or_else(|| format!("Joined cube '{}' not found", spec.cube))?;

    let key: String = match &spec.using {
        Some(col) => {
            let in_primary = primary
                .dimension_names
                .iter()
                .any(|d| d.eq_ignore_ascii_case(col));
            let in_joined = joined
                .dimension_names
                .iter()
                .any(|d| d.eq_ignore_ascii_case(col));
            if !in_primary || !in_joined {
                return Err(format!(
                    "Cannot JOIN '{}' with '{}' USING '{}': it must be a conformed \
                     dimension (present in both cubes).",
                    primary_name, spec.cube, col
                ));
            }
            // Normalise to the PRIMARY cube's exact casing. The check above guarantees
            // a match; resolving without `unwrap` means a future change to that guard
            // cannot turn this into a panic.
            primary
                .dimension_names
                .iter()
                .find(|d| d.eq_ignore_ascii_case(col))
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "Cannot JOIN '{}' with '{}' USING '{}': it must be a conformed \
                         dimension (present in both cubes).",
                        primary_name, spec.cube, col
                    )
                })?
        }
        None => {
            let shared: Vec<String> = primary
                .dimension_names
                .iter()
                .filter(|d| joined.dimension_names.contains(*d))
                .cloned()
                .collect();
            match shared.len() {
                0 => {
                    return Err(format!(
                        "Cannot JOIN '{}' with '{}': they share no conformed dimension. \
                         A JOIN keys off a dimension with the same name in both cubes; \
                         write JOIN ... USING (<dimension>).",
                        primary_name, spec.cube
                    ));
                }
                1 => shared.into_iter().next().unwrap(),
                _ => {
                    return Err(format!(
                        "Cannot infer how to JOIN '{}' with '{}': they share {} conformed \
                         dimensions ({}). Specify one with USING (<dimension>).",
                        primary_name,
                        spec.cube,
                        shared.len(),
                        shared.join(", ")
                    ));
                }
            }
        }
    };

    Ok(ResolvedJoin {
        primary: primary_name.to_string(),
        cube: spec.cube.clone(),
        key,
    })
}

/// Attaches `attributes` from the joined cube onto `primary`, in place.
///
/// One query against the attribute cube for ALL keys (not one per row), then an
/// in-memory index — the same "bulk method" the SQL path used, now shared.
///
/// Attribute columns that are not already present are APPENDED at the end, in the
/// order given. An attribute that is not a member of the joined cube's measure
/// dimension is an error: it used to produce a column of dashes, which reads as
/// "every row is missing this attribute" rather than "you asked for the wrong name".
pub fn apply(
    catalog: &Catalog,
    primary: &mut ResultSet,
    resolved: &ResolvedJoin,
    attributes: &[String],
    missing: &str,
) -> Result<(), String> {
    if attributes.is_empty() {
        return Ok(());
    }

    let attribute_cube = catalog
        .get_cube(&resolved.cube)
        .ok_or_else(|| format!("Joined cube '{}' not found", resolved.cube))?;

    // The key must be projected: without it there is nothing to join against.
    let key_idx = primary
        .headers
        .iter()
        .position(|h| h == &resolved.key)
        .ok_or_else(|| {
            format!(
                "Cannot JOIN '{}' with '{}': the conformed dimension '{}' is not projected, \
                 so there is no key to join the attributes onto.",
                resolved.primary, resolved.cube, resolved.key
            )
        })?;

    // Every requested attribute must exist in the joined cube's measure dimension.
    if let Some(measure_dim) = &attribute_cube.measure_dimension
        && let Some(i) = attribute_cube
            .dimension_names
            .iter()
            .position(|n| n == measure_dim)
    {
        let dim = attribute_cube.dimensions[i].read().unwrap();
        for attribute in attributes {
            if dim.get_id(attribute).is_none() {
                return Err(format!(
                    "Cannot JOIN '{}' with '{}': '{}' is not an attribute of '{}'.",
                    resolved.primary, resolved.cube, attribute, resolved.cube
                ));
            }
        }
    }

    // Read the attribute cells DIRECTLY, keyed by the conformed dimension.
    //
    // Not through `query_slice`: that fills a missing measure cell with "-", so a key
    // that simply has no `Code` would arrive as the literal string "-" and be
    // indistinguishable from a real value of "-". Scanning the store keeps "absent"
    // and "empty" distinct, which is what lets the wire layer emit `null`. Attribute
    // cubes are flat (non-aggregating), so the raw cells are the values.
    let attr_key_idx = attribute_cube
        .dimension_names
        .iter()
        .position(|d| d == &resolved.key)
        .ok_or_else(|| {
            format!(
                "Cannot JOIN '{}' with '{}': '{}' is not a dimension of '{}'.",
                resolved.primary, resolved.cube, resolved.key, resolved.cube
            )
        })?;
    let attr_value_idx = attribute_cube
        .measure_dimension
        .as_ref()
        .and_then(|m| attribute_cube.dimension_names.iter().position(|n| n == m))
        .ok_or_else(|| {
            format!(
                "Cannot JOIN '{}' with '{}': it has no measure dimension, so it holds no \
                 attributes.",
                resolved.primary, resolved.cube
            )
        })?;

    let wants: std::collections::HashSet<&str> = attributes.iter().map(String::as_str).collect();
    let mut index: HashMap<String, HashMap<String, String>> = HashMap::new();
    {
        let dim_count = attribute_cube.dimensions.len();
        let cells = attribute_cube.store.scan_subcube(&vec![None; dim_count]);
        let key_dim = attribute_cube.dimensions[attr_key_idx].read().unwrap();
        let attribute_dim = attribute_cube.dimensions[attr_value_idx].read().unwrap();
        for (coords, value) in &cells {
            let attribute = attribute_dim.get_name(coords[attr_value_idx]);
            if !wants.contains(attribute.as_str()) {
                continue;
            }
            let key = key_dim.get_name(coords[attr_key_idx]);
            let cell = match value {
                CellValue::Numeric(n) => format!("{n}"),
                CellValue::String(s) => s.clone(),
            };
            index.entry(key).or_default().insert(attribute, cell);
        }
    }

    // Ensure every attribute has a column. SQL listed them in SELECT (so they are
    // already headers); the plan layer did not, and they are appended here.
    for attribute in attributes {
        if !primary.headers.iter().any(|h| h == attribute) {
            primary.headers.push(attribute.clone());
            for row in &mut primary.rows {
                row.push(missing.to_string());
            }
        }
    }
    let width = primary.headers.len();
    let targets: Vec<usize> = attributes
        .iter()
        .map(|a| primary.headers.iter().position(|h| h == a).unwrap_or(0))
        .collect();

    for row in &mut primary.rows {
        row.resize(width, missing.to_string());
        let key = row.get(key_idx).cloned().unwrap_or_default();
        let values = index.get(&key);
        for (attribute, &out_idx) in attributes.iter().zip(targets.iter()) {
            let cell = values
                .and_then(|m| m.get(attribute))
                .cloned()
                .unwrap_or_else(|| missing.to_string());
            if out_idx < row.len() {
                row[out_idx] = cell;
            }
        }
    }

    Ok(())
}
