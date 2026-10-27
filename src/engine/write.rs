//! Writeback: batched cell writes (WRITE-API-DESIGN.md).
//!
//! MECHANICAL ONLY. Permission checks (table WRITE level, dimension visibility, autocreate
//! governance) are the TRANSPORT's job: it has the session, this module has the catalogue.
//! Keeping the split means the engine can also be used for unrestricted bulk loading from
//! the CLI without pretending that is a user action.
//!
//! CLIENT-CONTROLLED BEHAVIOUR (the user's decision):
//!   * `splash` chooses what a write to a CONSOLIDATED coordinate means: refuse it
//!     (`reject`, the default), or distribute it by rollup weights (`replace` / `add`).
//!   * `createMissing` chooses whether an unknown member is created. Strict by default:
//!     a misspelled member should not become part of the model because of a cell edit.
//!
//! ATOMICITY: every cell is validated before any cell is written, so a rejected batch
//! changes nothing. The engine has no transactions; a batch is the transaction.
//!
//! WHY `Cube::write` IS NOT ENOUGH on its own: it calls `Dimension::add_leaf`, which for an
//! existing CONSOLIDATED name creates a separate leaf that shadows the rollup. Splashing
//! therefore has to be chosen explicitly here, never left to `write`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::catalog::catalog::Catalog;
use crate::cube::cube::SplashMode;
use crate::cube::node::CellValue;

/// The largest batch accepted in one request.
///
/// A spreadsheet page is small; this exists so a malformed or hostile request cannot hold the
/// store lock for an unbounded time.
pub const MAX_CELLS_PER_REQUEST: usize = 10_000;

/// What a write to a consolidated coordinate does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Splash {
    /// Refuse the write: the caller learns the cell is not a leaf.
    Reject,
    /// Distribute so the rollup total becomes the value.
    Replace,
    /// Distribute the value as a delta on top of the current leaves.
    Add,
}

impl Splash {
    pub fn parse(text: Option<&str>) -> Result<Self, WriteError> {
        match text {
            None => Ok(Self::Reject),
            Some(value) => match value.to_ascii_lowercase().as_str() {
                "reject" => Ok(Self::Reject),
                "replace" => Ok(Self::Replace),
                "add" => Ok(Self::Add),
                other => Err(WriteError::invalid(format!(
                    "unknown splash mode '{other}'; expected reject, replace or add"
                ))),
            },
        }
    }

    fn mode(self) -> Option<SplashMode> {
        match self {
            Self::Reject => None,
            Self::Replace => Some(SplashMode::Replace),
            Self::Add => Some(SplashMode::Add),
        }
    }
}

/// One cell in a write request: coordinates by DIMENSION NAME, plus a value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellWrite {
    /// `{ "Region": "East", "Measure": "Sales" }`. A map, not a positional tuple, so the
    /// request does not depend on the client knowing the cube's dimension order.
    pub coords: HashMap<String, String>,
    /// A number, or a string for attribute tables.
    pub value: serde_json::Value,
}

/// A batched write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRequest {
    pub cells: Vec<CellWrite>,
    /// `reject` (default) | `replace` | `add` — see [`Splash`].
    #[serde(default)]
    pub splash: Option<String>,
    /// Create members that do not exist yet. Default false (strict).
    #[serde(rename = "createMissing", default)]
    pub create_missing: Option<bool>,
}

/// The result of an applied batch.
#[derive(Debug, Clone, Serialize)]
pub struct WriteResponse {
    pub written: usize,
    /// The cube's data version after the write. INFORMATIONAL: the client contract is
    /// last-write-wins, so this is not a precondition for the next write.
    pub version: u64,
}

/// A write failure, in the wire's `{error, code}` vocabulary.
#[derive(Debug, Clone)]
pub struct WriteError {
    pub code: &'static str,
    pub message: String,
}

impl WriteError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_argument",
            message: message.into(),
        }
    }

    pub fn unknown_dataset(dataset: &str) -> Self {
        Self {
            code: "unknown_dataset",
            message: format!("Dataset '{dataset}' not found."),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            code: "forbidden",
            message: message.into(),
        }
    }

    /// The HTTP status a write failure deserves.
    ///
    /// Reads answer 404 for a hidden dataset so structure does not leak; a WRITE must be
    /// honest instead: a silently "successful" write would be worse than admitting denial.
    pub fn status(&self) -> u16 {
        match self.code {
            "unknown_dataset" => 404,
            "forbidden" => 403,
            _ => 400,
        }
    }
}

/// Applies a batch. Validation first, then writes: all-or-nothing in practice.
pub fn write_cells(
    catalog: &Catalog,
    dataset: &str,
    request: &WriteRequest,
) -> Result<WriteResponse, WriteError> {
    let Some(cube) = catalog.get_cube(dataset) else {
        return Err(WriteError::unknown_dataset(dataset));
    };
    if crate::catalog::system_cubes::is_derived(&cube.name) {
        return Err(WriteError::invalid(format!(
            "'{}' is a derived system cube: it is read-only because the engine rebuilds it, \
             so a write would be lost.",
            cube.name
        )));
    }
    // SECURITY CONTROL CUBES ARE NOT DATA. Writing `#TableSecurity` through this endpoint
    // would let any role with default-accept write access grant itself permissions, so the
    // endpoint refuses them outright. They are changed through an admin SQL session (the
    // CLI), which is a different authority from "may write cells".
    if crate::catalog::system_cubes::is_control(&cube.name) {
        return Err(WriteError::forbidden(format!(
            "'{}' is a security control cube: it is changed through an admin SQL session, \
             not through the cell write API.",
            cube.name
        )));
    }
    if request.cells.is_empty() {
        return Err(WriteError::invalid("the write batch is empty"));
    }
    if request.cells.len() > MAX_CELLS_PER_REQUEST {
        return Err(WriteError::invalid(format!(
            "at most {MAX_CELLS_PER_REQUEST} cells per request; got {}",
            request.cells.len()
        )));
    }

    let splash = Splash::parse(request.splash.as_deref())?;
    let create_missing = request.create_missing.unwrap_or(false);

    // --- 1. VALIDATE EVERYTHING before writing anything ---
    let mut planned: Vec<(Vec<String>, CellValue)> = Vec::with_capacity(request.cells.len());
    for (index, cell) in request.cells.iter().enumerate() {
        let position = index + 1;
        let names = resolve_coordinates(cube, &cell.coords, position)?;
        let value = cell_value(&cell.value, position)?;

        // Consolidated coordinates: only the splash path may touch them.
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let consolidated = cube.has_consolidated_coordinate(&refs);
        if consolidated && splash.mode().is_none() {
            return Err(WriteError {
                code: "consolidated_write",
                message: format!(
                    "cell {position} is a consolidated coordinate ({}); send \"splash\": \
                     \"replace\" or \"add\", or write a leaf",
                    names.join(" / ")
                ),
            });
        }

        // Member existence, when the client did not opt into creating them.
        if !create_missing {
            for (dimension_index, member) in names.iter().enumerate() {
                let dim = cube.dimensions[dimension_index]
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if dim.get_id_in(None, member).is_none() {
                    return Err(WriteError {
                        code: "unknown_member",
                        message: format!(
                            "cell {position}: member '{member}' does not exist in dimension \
                             '{}'; set \"createMissing\": true to create it",
                            cube.dimension_names[dimension_index]
                        ),
                    });
                }
            }
        }

        // Splashing is arithmetic: it needs a number.
        if consolidated && !matches!(value, CellValue::Numeric(_)) {
            return Err(WriteError::invalid(format!(
                "cell {position}: a consolidated write needs a numeric value to distribute"
            )));
        }
        planned.push((names, value));
    }

    // --- 2. APPLY ---
    for (names, value) in &planned {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        if cube.has_consolidated_coordinate(&refs) {
            let CellValue::Numeric(amount) = value else {
                // Unreachable: validated above.
                return Err(WriteError::invalid("consolidated write needs a numeric value"));
            };
            let mode = splash.mode().unwrap_or(SplashMode::Replace);
            cube.write_splashed(&refs, *amount, mode)
                .map_err(WriteError::invalid)?;
        } else {
            cube.write(&refs, value.clone()).map_err(WriteError::invalid)?;
        }
    }

    Ok(WriteResponse {
        written: planned.len(),
        version: cube.store.version(),
    })
}

/// Places the request's coordinates into the cube's dimension order.
fn resolve_coordinates(
    cube: &crate::cube::cube::Cube,
    coords: &HashMap<String, String>,
    position: usize,
) -> Result<Vec<String>, WriteError> {
    let expected = cube.dimension_names.len();
    if coords.len() != expected {
        return Err(WriteError::invalid(format!(
            "cell {position}: expected {expected} coordinate(s), got {}; dimensions are {:?}",
            coords.len(),
            cube.dimension_names
        )));
    }

    let mut names = Vec::with_capacity(expected);
    for dimension in &cube.dimension_names {
        let found = coords
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(dimension))
            .map(|(_, value)| value.clone());
        match found {
            Some(member) if !member.trim().is_empty() => names.push(member),
            Some(_) => {
                return Err(WriteError::invalid(format!(
                    "cell {position}: empty member for dimension '{dimension}'"
                )));
            }
            None => {
                return Err(WriteError::invalid(format!(
                    "cell {position}: missing coordinate for dimension '{dimension}'; \
                     expected keys {:?}",
                    cube.dimension_names
                )));
            }
        }
    }
    Ok(names)
}

/// Converts a JSON value to a cell value.
fn cell_value(value: &serde_json::Value, position: usize) -> Result<CellValue, WriteError> {
    match value {
        serde_json::Value::Number(number) => number
            .as_f64()
            .map(CellValue::Numeric)
            .ok_or_else(|| WriteError::invalid(format!("cell {position}: unrepresentable number"))),
        serde_json::Value::String(text) => Ok(CellValue::String(text.clone())),
        other => Err(WriteError::invalid(format!(
            "cell {position}: value must be a number or a string, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog.add_cube("Sales", &["Region", "Product", "Measure"], Some("Measure"), true);
        catalog.add_cube("Attr", &["Region", "Attribute"], Some("Attribute"), false);
        catalog.add_cube("Other", &["Anything"], None, true);
        catalog
    }

    /// The same catalogue with the members a normal model would already have.
    ///
    /// A fresh catalogue has NO members, so strict writes would be refused - which is the
    /// correct behaviour, and exactly why these tests seed first.
    fn catalog_with_members() -> Catalog {
        let catalog = catalog();
        for (dimension, members) in [
            ("Region", vec!["East", "West"]),
            ("Product", vec!["Widget"]),
            ("Measure", vec!["Sales", "Cost"]),
            ("Attribute", vec!["Owner"]),
            ("Anything", vec!["x"]),
        ] {
            let dim = catalog.get_dimension(dimension).expect("dimension");
            let mut dim = dim.write().expect("lock");
            for member in members {
                dim.add_leaf(member);
            }
        }
        catalog
    }

    fn write_one(
        catalog: &Catalog,
        dataset: &str,
        coords: &[(&str, &str)],
        value: serde_json::Value,
        splash: Option<&str>,
        create_missing: bool,
    ) -> Result<WriteResponse, WriteError> {
        let request = WriteRequest {
            cells: vec![CellWrite {
                coords: coords
                    .iter()
                    .map(|(dimension, member)| (dimension.to_string(), member.to_string()))
                    .collect(),
                value,
            }],
            splash: splash.map(str::to_string),
            create_missing: Some(create_missing),
        };
        write_cells(catalog, dataset, &request)
    }

    fn read(catalog: &Catalog, coords: &[&str]) -> Option<CellValue> {
        catalog.get_cube("Sales")?.read_cell(coords)
    }

    #[test]
    fn a_leaf_write_lands_and_is_readable() {
        let catalog = catalog_with_members();
        write_one(
            &catalog,
            "Sales",
            &[("Region", "East"), ("Product", "Widget"), ("Measure", "Sales")],
            serde_json::json!(42.5),
            None,
            false,
        )
        .expect("write");
        assert_eq!(read(&catalog, &["East", "Widget", "Sales"]), Some(CellValue::Numeric(42.5)));
    }

    #[test]
    fn string_values_write_to_attribute_cubes() {
        let catalog = catalog_with_members();
        write_one(
            &catalog,
            "Attr",
            &[("Region", "East"), ("Attribute", "Owner")],
            serde_json::json!("Alice"),
            None,
            false,
        )
        .expect("write");
        let cube = catalog.get_cube("Attr").expect("Attr");
        assert_eq!(
            cube.read_cell(&["East", "Owner"]),
            Some(CellValue::String("Alice".to_string()))
        );
    }

    #[test]
    fn coordinates_are_matched_case_insensitively_against_dimension_names() {
        let catalog = catalog_with_members();
        write_one(
            &catalog,
            "Sales",
            &[("region", "East"), ("PRODUCT", "Widget"), ("Measure", "Sales")],
            serde_json::json!(1.0),
            None,
            false,
        )
        .expect("dimension names are case-insensitive");
        assert_eq!(read(&catalog, &["East", "Widget", "Sales"]), Some(CellValue::Numeric(1.0)));
    }

    #[test]
    fn a_missing_coordinate_is_an_invalid_argument() {
        let catalog = catalog_with_members();
        let err = write_one(
            &catalog,
            "Sales",
            &[("Region", "East"), ("Product", "Widget")],
            serde_json::json!(1.0),
            None,
            false,
        )
        .expect_err("arity mismatch");
        assert_eq!(err.code, "invalid_argument");
        assert!(err.message.contains("expected 3"), "{}", err.message);
    }

    #[test]
    fn a_misspelled_member_is_refused_by_default() {
        let catalog = catalog_with_members();
        let err = write_one(
            &catalog,
            "Sales",
            &[("Region", "East"), ("Product", "Widgets"), ("Measure", "Sales")],
            serde_json::json!(1.0),
            None,
            false,
        )
        .expect_err("strict by default");
        assert_eq!(err.code, "unknown_member");
        assert!(err.message.contains("createMissing"), "{}", err.message);
    }

    #[test]
    fn create_missing_creates_the_member_when_asked() {
        let catalog = catalog_with_members();
        write_one(
            &catalog,
            "Sales",
            &[("Region", "North"), ("Product", "Widget"), ("Measure", "Sales")],
            serde_json::json!(7.0),
            None,
            true,
        )
        .expect("createMissing");
        assert_eq!(read(&catalog, &["North", "Widget", "Sales"]), Some(CellValue::Numeric(7.0)));
    }

    #[test]
    fn writing_a_consolidated_cell_is_refused_without_a_splash_mode() {
        let catalog = catalog_with_members();
        {
            let cube = catalog.get_cube("Sales").expect("Sales");
            cube.write(&["East", "Widget", "Sales"], CellValue::Numeric(1.0)).expect("leaf");
            cube.write(&["West", "Widget", "Sales"], CellValue::Numeric(3.0)).expect("leaf");
        }
        // Consolidate East + West under "All" on the Region dimension.
        {
            let dim = catalog.get_dimension("Region").expect("Region");
            let mut dim = dim.write().expect("lock");
            // Consolidate in the DEFAULT hierarchy: plain coordinates resolve there.
            dim.add_component_in(None, "Total", "East", 1.0).expect("East");
            dim.add_component_in(None, "Total", "West", 1.0).expect("West");
        }

        let err = write_one(
            &catalog,
            "Sales",
            &[("Region", "Total"), ("Product", "Widget"), ("Measure", "Sales")],
            serde_json::json!(10.0),
            None,
            false,
        )
        .expect_err("consolidated write must be explicit");
        assert_eq!(err.code, "consolidated_write");

        // Nothing was written.
        assert_eq!(read(&catalog, &["East", "Widget", "Sales"]), Some(CellValue::Numeric(1.0)));
    }

    #[test]
    fn splash_replace_sets_the_rollup_total() {
        let catalog = catalog_with_members();
        {
            let cube = catalog.get_cube("Sales").expect("Sales");
            cube.write(&["East", "Widget", "Sales"], CellValue::Numeric(1.0)).expect("leaf");
            cube.write(&["West", "Widget", "Sales"], CellValue::Numeric(3.0)).expect("leaf");
            let dim = catalog.get_dimension("Region").expect("Region");
            let mut dim = dim.write().expect("lock");
            // Consolidate in the DEFAULT hierarchy: plain coordinates resolve there.
            dim.add_component_in(None, "Total", "East", 1.0).expect("East");
            dim.add_component_in(None, "Total", "West", 1.0).expect("West");
        }

        write_one(
            &catalog,
            "Sales",
            &[("Region", "Total"), ("Product", "Widget"), ("Measure", "Sales")],
            serde_json::json!(100.0),
            Some("replace"),
            false,
        )
        .expect("splash replace");

        // Even split: 50 each.
        assert_eq!(read(&catalog, &["East", "Widget", "Sales"]), Some(CellValue::Numeric(50.0)));
        assert_eq!(read(&catalog, &["West", "Widget", "Sales"]), Some(CellValue::Numeric(50.0)));
    }

    #[test]
    fn splash_on_a_leaf_behaves_like_a_plain_write() {
        let catalog = catalog_with_members();
        write_one(
            &catalog,
            "Sales",
            &[("Region", "East"), ("Product", "Widget"), ("Measure", "Sales")],
            serde_json::json!(5.0),
            Some("add"),
            false,
        )
        .expect("splash is harmless on a leaf");
        assert_eq!(read(&catalog, &["East", "Widget", "Sales"]), Some(CellValue::Numeric(5.0)));
    }

    #[test]
    fn an_unknown_splash_mode_is_refused() {
        let catalog = catalog_with_members();
        let err = write_one(
            &catalog,
            "Sales",
            &[("Region", "East"), ("Product", "Widget"), ("Measure", "Sales")],
            serde_json::json!(1.0),
            Some("spread"),
            false,
        )
        .expect_err("unknown mode");
        assert_eq!(err.code, "invalid_argument");
        assert!(err.message.contains("spread"), "{}", err.message);
    }

    #[test]
    fn a_batch_is_atomic_when_any_cell_is_invalid() {
        let catalog = catalog_with_members();
        let request = WriteRequest {
            cells: vec![
                CellWrite {
                    coords: HashMap::from([
                        ("Region".to_string(), "East".to_string()),
                        ("Product".to_string(), "Widget".to_string()),
                        ("Measure".to_string(), "Sales".to_string()),
                    ]),
                    value: serde_json::json!(1.0),
                },
                CellWrite {
                    coords: HashMap::from([
                        ("Region".to_string(), "East".to_string()),
                        ("Product".to_string(), "Widget".to_string()),
                        ("Measure".to_string(), "Missing".to_string()),
                    ]),
                    value: serde_json::json!(2.0),
                },
            ],
            splash: None,
            create_missing: Some(false),
        };
        let err = write_cells(&catalog, "Sales", &request).expect_err("batch rejected");
        assert_eq!(err.code, "unknown_member");
        assert_eq!(
            read(&catalog, &["East", "Widget", "Sales"]),
            None,
            "the valid cell must NOT have been written"
        );
    }

    /// The control cubes are engine CONFIGURATION: the data write API must not touch them, or
    /// a role with default-accept write access could grant itself permissions.
    #[test]
    fn a_security_control_cube_is_not_writable_through_the_data_api() {
        let mut catalog = catalog_with_members();
        crate::catalog::system_cubes::rebuild(&mut catalog);
        for cube in ["#TableSecurity", "#DimensionSecurity"] {
            let err = match cube {
                "#TableSecurity" => write_one(
                    &catalog,
                    cube,
                    &[("#Role", "reader"), ("#Cube", "Sales")],
                    serde_json::json!("WRITE"),
                    None,
                    true,
                ),
                _ => write_one(
                    &catalog,
                    cube,
                    &[("#Role", "reader"), ("#Dimension", "Region"), ("#Member", "#ALL")],
                    serde_json::json!("WRITE"),
                    None,
                    true,
                ),
            }
            .expect_err("control cubes are not data");
            assert_eq!(err.code, "forbidden", "{cube}: {}", err.message);
        }
    }

    #[test]
    fn a_derived_system_cube_is_read_only() {
        let mut catalog = catalog();
        crate::catalog::system_cubes::rebuild(&mut catalog);
        let err = write_one(
            &catalog,
            "#Cubes",
            &[("#Cube", "Sales"), ("#Property", "measureDimension")],
            serde_json::json!("x"),
            None,
            false,
        )
        .expect_err("derived cube");
        assert_eq!(err.code, "invalid_argument");
    }

    #[test]
    fn an_unknown_dataset_is_a_404() {
        let catalog = catalog_with_members();
        let err = write_one(
            &catalog,
            "Nope",
            &[("Anything", "x")],
            serde_json::json!(1.0),
            None,
            false,
        )
        .expect_err("unknown dataset");
        assert_eq!(err.code, "unknown_dataset");
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn an_empty_batch_is_refused() {
        let catalog = catalog_with_members();
        let request = WriteRequest {
            cells: Vec::new(),
            splash: None,
            create_missing: None,
        };
        assert_eq!(write_cells(&catalog, "Sales", &request).unwrap_err().code, "invalid_argument");
    }
}
