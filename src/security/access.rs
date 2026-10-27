//! Table access: which cubes a session's roles may see (SECURITY-DESIGN.md Â§3, S2).
//!
//! WHERE THE RULES LIVE
//!   In the `#TableSecurity` CONTROL cube, `(#Role, #Cube) -> READ | WRITE | NONE`. It is an
//!   ordinary cube, so it is persisted, versioned and queryable like everything else, and an
//!   admin writes it with plain SQL:
//!
//!   ```text
//!   INSERT INTO #TableSecurity VALUES ('reader', 'Sales', 'NONE')
//!   ```
//!
//!   The engine adds that cube at load and rebuild time (without overwriting it).
//!
//! DEFAULT IS ACCEPT (the user's Infor-style decision, SECURITY-DESIGN.md Â§1). A cube with no
//! rule is fully accessible. A rule exists to RESTRICT.
//!
//! WHAT THAT MEANS FOR SEVERAL ROLES
//!   The most restrictive EXPLICIT rule wins. A role with no rule for the cube does not raise
//!   the level back to ACCEPT: with default-accept, "union across roles" would mean any
//!   rule-less role silently overrides every restriction, and `NONE` could never bind. So
//!   adding a role can only narrow, which is also the safe direction.
//!
//! WHAT A RESTRICTION LOOKS LIKE ON THE WIRE
//!   Nothing. A cube the session may not read is reported exactly like a cube that does not
//!   exist (`unknown_dataset`, 404), and it is omitted from `GET /v1/datasets`. The client is
//!   not told that a hidden cube exists - that would leak structure, and the design calls for
//!   no silent leaks.

use std::collections::HashMap;

use crate::catalog::catalog::Catalog;
use crate::catalog::system_cubes::TABLE_SECURITY;
use crate::cube::node::CellValue;

/// A table access level. Ordered so `min` means "most restrictive".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// The cube is invisible to this role.
    Forbidden,
    /// May read and aggregate, may not write.
    Read,
    /// May read and write. (No HTTP write endpoint exists yet; recorded for S4.)
    Write,
}

impl Level {
    /// Parses a stored level. Case-insensitive, and `NONE` is accepted as a synonym for
    /// `FORBIDDEN` because that is the word the design and Infor use.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "NONE" | "FORBIDDEN" => Some(Self::Forbidden),
            "READ" => Some(Self::Read),
            "WRITE" => Some(Self::Write),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Forbidden => "NONE",
            Self::Read => "READ",
            Self::Write => "WRITE",
        }
    }
}

/// The effective table access for one session.
#[derive(Debug, Clone, Default)]
pub struct TableAccess {
    /// Cube name (lowercased, the catalogue's key form) -> most restrictive level.
    rules: HashMap<String, Level>,
}

impl TableAccess {
    /// No rules at all: everything allowed. This is what an anonymous/development request
    /// gets, and it is also the state before any security cube has content.
    pub fn open() -> Self {
        Self::default()
    }

    /// Builds the policy for a set of roles from the `#TableSecurity` cube.
    ///
    /// An empty role set yields `open()`: with no identity there is nothing to restrict on,
    /// and the CALLER is responsible for refusing unauthenticated requests (which the server
    /// does). This function must never be used as the authentication check.
    pub fn for_roles(catalog: &Catalog, roles: &[String]) -> Self {
        if roles.is_empty() {
            return Self::open();
        }
        let Some(cube) = catalog.get_cube(TABLE_SECURITY) else {
            return Self::open();
        };

        let mut rules: HashMap<String, Level> = HashMap::new();
        for (coords, value) in cube.store.scan_subcube(&vec![None; cube.dimensions.len()]) {
            let field = |index: usize| {
                cube.dimensions
                    .get(index)
                    .and_then(|dim| dim.read().ok().map(|d| d.get_name(coords[index])))
            };
            let (Some(role), Some(cube_name)) = (field(0), field(1)) else {
                continue;
            };
            if !roles.iter().any(|held| held.eq_ignore_ascii_case(&role)) {
                continue;
            }
            let Some(level) = Level::parse(&value.to_string()) else {
                // An unreadable level must FAIL CLOSED for a restriction-flavoured system:
                // a typo in an ACL should hide the cube, not open it.
                rules
                    .entry(cube_name.to_ascii_lowercase())
                    .and_modify(|existing| *existing = (*existing).min(Level::Forbidden))
                    .or_insert(Level::Forbidden);
                continue;
            };
            rules
                .entry(cube_name.to_ascii_lowercase())
                .and_modify(|existing| *existing = (*existing).min(level))
                .or_insert(level);
        }
        Self { rules }
    }

    /// The level for one cube. DEFAULT ACCEPT: no rule means full access.
    pub fn level_for(&self, cube: &str) -> Level {
        self.rules
            .get(&cube.to_ascii_lowercase())
            .copied()
            .unwrap_or(Level::Write)
    }

    /// Whether the session may read a cube.
    pub fn can_read(&self, cube: &str) -> bool {
        self.level_for(cube) >= Level::Read
    }

    /// Number of rules this policy holds (for logging and tests).
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// Writes one rule into the control cube. This is the admin path (SQL `INSERT` does the same
/// thing); it exists so the CLI and tests do not have to know the cube's shape.
pub fn set_rule(catalog: &mut Catalog, role: &str, cube_name: &str, level: Level) -> Result<(), String> {
    let Some(cube) = catalog.get_cube_mut(TABLE_SECURITY) else {
        return Err(format!(
            "'{TABLE_SECURITY}' does not exist; the catalogue was not loaded through the \
             normal path"
        ));
    };
    cube.write(&[role, cube_name], CellValue::String(level.name().to_string()))
}

// NOTE: there is no `clear_rule`. The engine has no single-cell removal API, and writing an
// empty level would be read as a malformed rule (which fails closed - hiding the cube). To
// restore the default instead, write `WRITE` explicitly:
//
//     INSERT INTO #TableSecurity VALUES ('reader', 'Sales', 'WRITE')
//
// That is why Level::parse has no "empty" case.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::catalog::Catalog;
    use crate::catalog::system_cubes;

    fn catalog_with_control_cube() -> Catalog {
        let mut catalog = Catalog::new();
        catalog.add_cube("Sales", &["Region"], Some("Region"), true);
        catalog.add_cube("Markets", &["Region"], Some("Region"), true);
        system_cubes::rebuild(&mut catalog);
        catalog
    }

    #[test]
    fn with_no_rules_everything_is_allowed() {
        let catalog = catalog_with_control_cube();
        let policy = TableAccess::for_roles(&catalog, &["reader".to_string()]);
        assert_eq!(policy.level_for("Sales"), Level::Write, "DEFAULT ACCEPT");
        assert!(policy.can_read("Sales"));
        assert_eq!(policy.rule_count(), 0);
    }

    #[test]
    fn an_explicit_none_hides_only_that_cube() {
        let mut catalog = catalog_with_control_cube();
        set_rule(&mut catalog, "reader", "Sales", Level::Forbidden).expect("rule");

        let policy = TableAccess::for_roles(&catalog, &["reader".to_string()]);
        assert_eq!(policy.level_for("Sales"), Level::Forbidden);
        assert!(!policy.can_read("Sales"));
        assert_eq!(policy.level_for("Markets"), Level::Write, "other cubes unaffected");
    }

    #[test]
    fn rules_for_other_roles_do_not_apply() {
        let mut catalog = catalog_with_control_cube();
        set_rule(&mut catalog, "other", "Sales", Level::Forbidden).expect("rule");

        let policy = TableAccess::for_roles(&catalog, &["reader".to_string()]);
        assert!(policy.can_read("Sales"), "another role's rule must not bind");
    }

    #[test]
    fn the_most_restrictive_held_role_wins() {
        let mut catalog = catalog_with_control_cube();
        set_rule(&mut catalog, "reader", "Sales", Level::Forbidden).expect("rule");

        // A second role with no rule must NOT raise the level back to ACCEPT: with
        // default-accept, that would make NONE impossible to enforce.
        let policy = TableAccess::for_roles(&catalog, &["reader".to_string(), "analyst".to_string()]);
        assert_eq!(policy.level_for("Sales"), Level::Forbidden);

        // But a second role with an explicit READ keeps it readable.
        set_rule(&mut catalog, "analyst", "Markets", Level::Read).expect("rule");
        let policy = TableAccess::for_roles(&catalog, &["analyst".to_string()]);
        assert_eq!(policy.level_for("Markets"), Level::Read);
    }

    #[test]
    fn cube_names_match_case_insensitively() {
        let mut catalog = catalog_with_control_cube();
        set_rule(&mut catalog, "reader", "salES", Level::Forbidden).expect("rule");
        let policy = TableAccess::for_roles(&catalog, &["Reader".to_string()]);
        assert!(!policy.can_read("Sales"));
        assert!(!policy.can_read("SALES"));
    }

    #[test]
    fn an_unreadable_level_fails_closed() {
        let mut catalog = catalog_with_control_cube();
        set_rule(&mut catalog, "reader", "Sales", Level::Read).expect("rule");
        // Overwrite with a typo by writing the cell directly.
        {
            let cube = catalog.get_cube_mut(TABLE_SECURITY).expect("cube");
            cube.write(&["reader", "Sales"], CellValue::String("REDA".to_string()))
                .expect("write");
        }
        let policy = TableAccess::for_roles(&catalog, &["reader".to_string()]);
        assert_eq!(
            policy.level_for("Sales"),
            Level::Forbidden,
            "a malformed ACL entry must hide the cube, not open it"
        );
    }

    #[test]
    fn level_parsing_is_case_insensitive_and_rejects_nonsense() {
        assert_eq!(Level::parse("read"), Some(Level::Read));
        assert_eq!(Level::parse(" WRITE "), Some(Level::Write));
        assert_eq!(Level::parse("none"), Some(Level::Forbidden));
        assert_eq!(Level::parse("nonsense"), None);
        assert_eq!(Level::parse(""), None);
    }

    #[test]
    fn a_control_cube_can_be_written() {
        // The derived cubes are read-only, but the whole point of #TableSecurity is that an
        // admin writes it.
        let mut catalog = catalog_with_control_cube();
        set_rule(&mut catalog, "reader", "Sales", Level::Forbidden).expect("write must work");

        let derived = catalog.get_cube("#Cubes").expect("#Cubes");
        assert!(
            derived.write(&["x", "y"], CellValue::String("z".to_string())).is_err(),
            "derived cubes stay read-only"
        );
    }
}

// ---------------------------------------------------------------------------
// Dimension access (S3)
// ---------------------------------------------------------------------------

/// The `#DimensionSecurity` control cube: `(#Role, #Dimension, #Member)` -> `NONE | READ | WRITE`.
pub const DIMENSION_SECURITY: &str = "#DimensionSecurity";

/// The member name meaning "the whole dimension".
///
/// The user chose `#ALL` in the design. A whole-dimension NONE hides every CUBE that uses the
/// dimension (a cube cannot be read without one of its axes), which is a cleaner answer than
/// silently emptying every aggregate.
pub const ALL_MEMBERS: &str = "#ALL";

/// Effective dimension access for one session.
///
/// LEVELS, not just denials: a member rule can be `NONE`, `READ` or `WRITE`, and a whole
/// dimension can be restricted with the `#ALL` member. Reads need `READ` or better; writes
/// need `WRITE`; CREATING a member needs the whole-dimension level to be `WRITE`, because a
/// per-member rule cannot exist for a member that does not exist yet. That last rule is how
/// S3 governs autocreate, as the user specified.
#[derive(Debug, Clone, Default)]
pub struct DimensionAccess {
    /// (dimension, member), both lowercased -> most restrictive level.
    members: std::collections::HashMap<(String, String), Level>,
    /// dimension, lowercased -> level for the WHOLE dimension (`#ALL` row).
    whole: std::collections::HashMap<String, Level>,
}

impl DimensionAccess {
    pub fn open() -> Self {
        Self::default()
    }

    /// Builds the policy for the roles a session holds.
    ///
    /// Most restrictive rule wins across held roles, for the same reason as table access:
    /// with default-accept, a permissive role would otherwise erase every restriction.
    pub fn for_roles(catalog: &Catalog, roles: &[String]) -> Self {
        if roles.is_empty() {
            return Self::open();
        }
        let Some(cube) = catalog.get_cube(DIMENSION_SECURITY) else {
            return Self::open();
        };

        let mut members: std::collections::HashMap<(String, String), Level> =
            std::collections::HashMap::new();
        let mut whole: std::collections::HashMap<String, Level> = std::collections::HashMap::new();

        for (coords, value) in cube.store.scan_subcube(&vec![None; cube.dimensions.len()]) {
            let field = |index: usize| {
                cube.dimensions
                    .get(index)
                    .and_then(|dim| dim.read().ok().map(|d| d.get_name(coords[index])))
            };
            let (Some(role), Some(dimension), Some(member)) = (field(0), field(1), field(2)) else {
                continue;
            };
            if !roles.iter().any(|held| held.eq_ignore_ascii_case(&role)) {
                continue;
            }
            // An unreadable level in an ACL entry must fail CLOSED.
            let level = Level::parse(&value.to_string()).unwrap_or(Level::Forbidden);

            let dimension_key = dimension.to_ascii_lowercase();
            if member.eq_ignore_ascii_case(ALL_MEMBERS) {
                whole
                    .entry(dimension_key)
                    .and_modify(|existing| *existing = (*existing).min(level))
                    .or_insert(level);
            } else {
                members
                    .entry((dimension_key, member.to_ascii_lowercase()))
                    .and_modify(|existing| *existing = (*existing).min(level))
                    .or_insert(level);
            }
        }

        Self { members, whole }
    }

    /// Dimensions denied in full.
    pub fn whole_dimensions(&self) -> Vec<String> {
        self.whole
            .iter()
            .filter(|(_, level)| **level == Level::Forbidden)
            .map(|(dimension, _)| dimension.clone())
            .collect()
    }

    /// True when this policy makes a CUBE unreadable because one of its dimensions is denied
    /// in full.
    pub fn hides_cube(&self, catalog: &Catalog, cube_name: &str) -> bool {
        let denied = self.whole_dimensions();
        if denied.is_empty() {
            return false;
        }
        let Some(cube) = catalog.get_cube(cube_name) else {
            return false;
        };
        cube.dimension_names.iter().any(|dim| {
            denied
                .iter()
                .any(|denied_dim| denied_dim.eq_ignore_ascii_case(dim))
        })
    }

    /// The denied member names, keyed by dimension name as the query layer knows it.
    ///
    /// Whole-dimension denials are NOT included here: they are handled by hiding the cube,
    /// so there is no cube left to filter.
    pub fn denied_members(&self) -> std::collections::HashMap<String, Vec<String>> {
        let mut denied: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for ((dimension, member), level) in &self.members {
            if *level == Level::Forbidden {
                denied.entry(dimension.clone()).or_default().push(member.clone());
            }
        }
        denied
    }

    /// The level for reading one member: the most specific rule, else the whole-dimension
    /// rule, else ACCEPT.
    pub fn read_level_for(&self, dimension: &str, member: &str) -> Level {
        let dimension_key = dimension.to_ascii_lowercase();
        if let Some(level) = self
            .members
            .get(&(dimension_key.clone(), member.to_ascii_lowercase()))
        {
            return *level;
        }
        self.whole.get(&dimension_key).copied().unwrap_or(Level::Write)
    }

    /// The level for WRITING one member. Same rule as reading today: a rule says what the
    /// role may do, and only `WRITE` permits a change.
    pub fn write_level_for(&self, dimension: &str, member: &str) -> Level {
        self.read_level_for(dimension, member)
    }

    /// Whether the role may CREATE a member in this dimension.
    ///
    /// Governed by the whole-dimension (`#ALL`) rule: a per-member rule cannot apply to a
    /// member that does not exist yet. Absent rule = ACCEPT.
    pub fn can_create_member(&self, dimension: &str) -> bool {
        self.whole
            .get(&dimension.to_ascii_lowercase())
            .copied()
            .unwrap_or(Level::Write)
            >= Level::Write
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.whole.is_empty()
    }

    pub fn denied_count(&self) -> usize {
        self.members
            .values()
            .filter(|level| **level == Level::Forbidden)
            .count()
    }
}

pub fn set_dimension_rule(
    catalog: &mut Catalog,
    role: &str,
    dimension: &str,
    member: &str,
    level: Level,
) -> Result<(), String> {
    let Some(cube) = catalog.get_cube_mut(DIMENSION_SECURITY) else {
        return Err(format!("'{DIMENSION_SECURITY}' does not exist"));
    };
    cube.write(&[role, dimension, member], CellValue::String(level.name().to_string()))
}

#[cfg(test)]
mod dimension_tests {
    use super::*;
    use crate::catalog::system_cubes;

    fn catalog_with_dimension_security() -> Catalog {
        let mut catalog = Catalog::new();
        catalog.add_cube("Sales", &["Region", "Product"], None, true);
        catalog.add_cube("Other", &["Anything"], None, true);
        system_cubes::rebuild(&mut catalog);
        catalog
    }

    #[test]
    fn with_no_rules_nothing_is_denied() {
        let catalog = catalog_with_dimension_security();
        let access = DimensionAccess::for_roles(&catalog, &["reader".to_string()]);
        assert!(access.is_empty());
        assert!(!access.hides_cube(&catalog, "Sales"));
    }

    #[test]
    fn a_member_rule_denies_only_that_member() {
        let mut catalog = catalog_with_dimension_security();
        set_dimension_rule(&mut catalog, "reader", "Region", "East", Level::Forbidden)
            .expect("rule");

        let access = DimensionAccess::for_roles(&catalog, &["reader".to_string()]);
        let denied = access.denied_members();
        assert_eq!(denied.get("region"), Some(&vec!["east".to_string()]));
        assert!(!access.hides_cube(&catalog, "Sales"), "the cube stays visible");
        assert_eq!(access.denied_count(), 1);
    }

    #[test]
    fn a_read_rule_does_not_deny_but_caps_writes() {
        let mut catalog = catalog_with_dimension_security();
        set_dimension_rule(&mut catalog, "reader", "Region", "East", Level::Read).expect("rule");
        let access = DimensionAccess::for_roles(&catalog, &["reader".to_string()]);

        // It denies nothing...
        assert!(access.denied_members().is_empty(), "READ is a grant, not a denial");
        assert_eq!(access.read_level_for("Region", "East"), Level::Read);
        // ...but it does not permit a write either.
        assert_eq!(access.write_level_for("Region", "East"), Level::Read);
        assert_eq!(access.denied_count(), 0);
    }

    #[test]
    fn the_all_wildcard_hides_cubes_using_the_dimension() {
        let mut catalog = catalog_with_dimension_security();
        set_dimension_rule(&mut catalog, "reader", "Region", ALL_MEMBERS, Level::Forbidden)
            .expect("rule");

        let access = DimensionAccess::for_roles(&catalog, &["reader".to_string()]);
        assert!(access.hides_cube(&catalog, "Sales"), "Sales has a Region axis");
        assert!(!access.hides_cube(&catalog, "Other"), "unrelated cube");
        assert!(access.denied_members().is_empty(), "handled by hiding the cube");
    }

    #[test]
    fn rules_for_other_roles_do_not_apply() {
        let mut catalog = catalog_with_dimension_security();
        set_dimension_rule(&mut catalog, "other", "Region", "East", Level::Forbidden)
            .expect("rule");
        let access = DimensionAccess::for_roles(&catalog, &["reader".to_string()]);
        assert!(access.is_empty());
    }

    #[test]
    fn a_denial_from_any_held_role_binds() {
        let mut catalog = catalog_with_dimension_security();
        set_dimension_rule(&mut catalog, "reader", "Region", "East", Level::Forbidden)
            .expect("rule");
        let access = DimensionAccess::for_roles(&catalog, &["reader".to_string(), "analyst".to_string()]);
        assert_eq!(access.denied_count(), 1);
    }

    #[test]
    fn a_control_cube_can_be_written() {
        let mut catalog = catalog_with_dimension_security();
        set_dimension_rule(&mut catalog, "reader", "Region", "East", Level::Forbidden)
            .expect("the control cube must be writable");
        let derived = catalog.get_cube("#Cubes").expect("#Cubes");
        assert!(
            derived.write(&["a", "b"], CellValue::String("c".to_string())).is_err(),
            "derived cubes stay read-only"
        );
    }
}
