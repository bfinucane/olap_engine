//! The aggregations the engine can actually compute.
//!
//! Storage is one value per leaf coordinate, so every aggregation here is a fold over
//! the leaf cells that contribute to a group — never over rows, which do not exist.
//!
//! SEMANTICS FIXED BY DECISION (see `AGGREGATIONS-DESIGN.md` §4):
//!   * Empty cells are never counted. `COUNT` counts non-empty cells; `AVG` divides by
//!     that count; `MIN`/`MAX` skip empties. `SUM` keeps the additive behaviour.
//!   * Non-additive aggregations use the BASE leaf value and IGNORE rollup weights, so
//!     on a ragged hierarchy `AVG != SUM / COUNT`. That is a difference in meaning, not
//!     a bug: a weight says how much a leaf contributes to a rollup, not how much it
//!     should count in a mean.
//!   * `MEDIAN`, `STDEV`, `VAR` and `COUNTD` are deliberately absent. The first three
//!     need all values per group (or a second pass); `COUNTD` needs a set per group.

/// One of the aggregations defined by ENGINE-PROTOCOL.md §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregation {
    Sum,
    Avg,
    Count,
    Min,
    Max,
}

impl Aggregation {
    /// Parses the protocol's spelling (case-insensitive). `None` means the engine does
    /// not implement it — the caller must refuse, never substitute SUM.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "SUM" => Some(Aggregation::Sum),
            "AVG" => Some(Aggregation::Avg),
            "COUNT" => Some(Aggregation::Count),
            "MIN" => Some(Aggregation::Min),
            "MAX" => Some(Aggregation::Max),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Aggregation::Sum => "SUM",
            Aggregation::Avg => "AVG",
            Aggregation::Count => "COUNT",
            Aggregation::Min => "MIN",
            Aggregation::Max => "MAX",
        }
    }

    /// Every aggregation this engine implements, for `capabilities.aggregations`.
    pub fn implemented() -> Vec<String> {
        [
            Aggregation::Sum,
            Aggregation::Avg,
            Aggregation::Count,
            Aggregation::Min,
            Aggregation::Max,
        ]
        .iter()
        .map(|a| a.name().to_string())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_implemented_aggregations_case_insensitively() {
        for (text, expected) in [
            ("SUM", Aggregation::Sum),
            ("sum", Aggregation::Sum),
            (" Avg ", Aggregation::Avg),
            ("COUNT", Aggregation::Count),
            ("MIN", Aggregation::Min),
            ("max", Aggregation::Max),
        ] {
            assert_eq!(Aggregation::parse(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn refuses_the_ones_it_cannot_compute() {
        for text in ["MEDIAN", "STDEV", "VAR", "COUNTD", "", "AVERAGE"] {
            assert_eq!(Aggregation::parse(text), None, "{text} must not parse");
        }
    }

    #[test]
    fn the_advertised_list_is_exactly_the_implemented_one() {
        let advertised = Aggregation::implemented();
        assert_eq!(advertised, vec!["SUM", "AVG", "COUNT", "MIN", "MAX"]);
        for name in &advertised {
            assert!(Aggregation::parse(name).is_some(), "{name} must round-trip");
        }
    }
}
