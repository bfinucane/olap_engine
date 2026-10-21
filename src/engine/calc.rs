//! Calculated fields: analysing and evaluating the formula AST.
//!
//! THE BOUNDARY: Plateau already parses the formula language into an AST whose tags
//! match `plan::AstNode` field for field, so the engine never needs a second grammar —
//! it evaluates what it is given. See `CALCULATIONS-DESIGN.md`.
//!
//! v1 evaluates, and nothing else:
//!   * aggregates `SUM/AVG/COUNT/MIN/MAX` over ONE field each,
//!   * arithmetic `+ - * / % ^` and unary `-`,
//!   * comparisons and `AND OR NOT`,
//!   * `IF(branches, otherwise)`,
//!   * `ZN` and `IFNULL`.
//!
//! SEMANTICS FIXED BY DECISION:
//!   * Division by zero is ABSENT, not 0 and not Infinity — a confident `0` margin is
//!     indistinguishable from a real one. `ZN(x)` is how a caller asks for 0 instead
//!     (the TM1 `\` idiom, expressed as a function so the choice is visible).
//!   * An absent operand makes the expression absent, except through `ZN`/`IFNULL`.
//!     Absence is the aggregation rule already agreed: a measure with no cells in a
//!     group is absent, not zero.

use crate::cube::aggregation::Aggregation;
use crate::engine::plan::AstNode;

/// The aggregates usable inside a formula, in the order the error message lists them.
pub const AGGREGATES: [&str; 5] = ["SUM", "AVG", "COUNT", "MIN", "MAX"];

/// The non-aggregate functions v1 implements. `capabilities.expressionFunctions`
/// advertises exactly this list, so it must never grow ahead of the implementation.
pub const FUNCTIONS: [&str; 2] = ["ZN", "IFNULL"];

fn aggregate_of(name: &str) -> Option<Aggregation> {
    AGGREGATES
        .iter()
        .find(|a| a.eq_ignore_ascii_case(name))
        .and_then(|a| Aggregation::parse(a))
}

fn unsupported_function(name: &str) -> String {
    format!(
        "function '{}' is not supported (implemented: {}, and the aggregates {})",
        name,
        FUNCTIONS.join(", "),
        AGGREGATES.join(", ")
    )
}

/// Walks an expression and collects the `(field, aggregation)` slots the query must
/// compute, rejecting anything v1 cannot evaluate.
///
/// One aggregation per field (decision 4): the slice result names a column by MEASURE,
/// so `SUM([Sales]) / AVG([Sales])` cannot be represented until that naming problem is
/// fixed for SQL and the plan layer together. Refused by name rather than silently
/// picking one.
pub fn collect_slots(expr: &AstNode, slots: &mut Vec<(String, Aggregation)>) -> Result<(), String> {
    match expr {
        AstNode::Call { name, args } => {
            if let Some(aggregation) = aggregate_of(name) {
                let [AstNode::Field { name: field }] = args.as_slice() else {
                    return Err(format!(
                        "{}() needs exactly one field argument, e.g. {}([Sales])",
                        name, name
                    ));
                };
                if let Some((_, existing)) = slots.iter().find(|(f, _)| f == field) {
                    if *existing != aggregation {
                        return Err(format!(
                            "field '{}' is aggregated twice, as {} and as {}; \
                             v1 supports one aggregation per field",
                            field,
                            existing.name(),
                            aggregation.name()
                        ));
                    }
                } else {
                    slots.push((field.clone(), aggregation));
                }
                Ok(())
            } else if FUNCTIONS.iter().any(|f| f.eq_ignore_ascii_case(name)) {
                for arg in args {
                    collect_slots(arg, slots)?;
                }
                Ok(())
            } else {
                Err(unsupported_function(name))
            }
        }
        AstNode::Lod { .. } => Err(
            "level-of-detail (LOD) expressions are not supported: FIXED/INCLUDE/EXCLUDE \
             compute at their own grouping, which is a second query, not an expression"
                .to_string(),
        ),
        // A bare field outside an aggregate would be a row-level reference, and there
        // are no rows here — only groups.
        AstNode::Field { name } => Err(format!(
            "'{}' is a row-level field reference; wrap it in an aggregate such as SUM([{}])",
            name, name
        )),
        AstNode::Unary { operand, .. } => collect_slots(operand, slots),
        AstNode::Binary { left, right, .. } => {
            collect_slots(left, slots)?;
            collect_slots(right, slots)
        }
        AstNode::If { branches, otherwise } => {
            for branch in branches {
                collect_slots(&branch.condition, slots)?;
                collect_slots(&branch.result, slots)?;
            }
            if let Some(otherwise) = otherwise {
                collect_slots(otherwise, slots)?;
            }
            Ok(())
        }
        // Literals and `null` contribute no slots.
        _ => Ok(()),
    }
}

/// A value during evaluation. Only numbers are needed as RESULTS; booleans exist for
/// conditions, and absence travels as `Null`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Val {
    Num(f64),
    Bool(bool),
    Null,
}

impl Val {
    /// A calculated MEASURE must produce a number or be absent. A boolean result is a
    /// mistake in the formula, not a 0/1 column.
    pub fn into_number(self) -> Result<Option<f64>, String> {
        match self {
            Val::Num(n) if n.is_finite() => Ok(Some(n)),
            Val::Num(_) => Ok(None),
            Val::Null => Ok(None),
            Val::Bool(_) => Err(
                "a calculated measure must produce a number, not a true/false value"
                    .to_string(),
            ),
        }
    }
}

/// Evaluates an expression. `lookup` returns the already-aggregated value of a field for
/// the current group (`None` = absent, the engine's normal answer for "no cells").
pub fn evaluate(expr: &AstNode, lookup: &dyn Fn(&str) -> Option<f64>) -> Result<Val, String> {
    match expr {
        AstNode::Number { value } => Ok(Val::Num(*value)),
        AstNode::Boolean { value } => Ok(Val::Bool(*value)),
        AstNode::Null => Ok(Val::Null),
        AstNode::String { .. } => Err(
            "string literals are not supported in a calculated measure yet".to_string(),
        ),

        AstNode::Field { name } => Err(format!(
            "'{}' is a row-level field reference; wrap it in an aggregate such as SUM([{}])",
            name, name
        )),

        AstNode::Call { name, args } => {
            if let Some(_aggregation) = aggregate_of(name) {
                let [AstNode::Field { name: field }] = args.as_slice() else {
                    return Err(format!("{}() needs exactly one field argument", name));
                };
                return Ok(match lookup(field) {
                    Some(value) => Val::Num(value),
                    None => Val::Null,
                });
            }

            if name.eq_ignore_ascii_case("ZN") {
                let [arg] = args.as_slice() else {
                    return Err("ZN() takes exactly one argument".to_string());
                };
                return Ok(match evaluate(arg, lookup)? {
                    Val::Null => Val::Num(0.0),
                    other => other,
                });
            }

            if name.eq_ignore_ascii_case("IFNULL") {
                let [value, fallback] = args.as_slice() else {
                    return Err("IFNULL() takes two arguments".to_string());
                };
                return Ok(match evaluate(value, lookup)? {
                    Val::Null => evaluate(fallback, lookup)?,
                    other => other,
                });
            }

            Err(unsupported_function(name))
        }

        AstNode::Unary { op, operand } => match (op.as_str(), evaluate(operand, lookup)?) {
            ("-", Val::Num(n)) => Ok(Val::Num(-n)),
            ("-", Val::Null) => Ok(Val::Null),
            ("NOT", Val::Bool(b)) => Ok(Val::Bool(!b)),
            ("NOT", Val::Null) => Ok(Val::Null),
            (other, _) => Err(format!("unsupported unary operator '{}'", other)),
        },

        AstNode::Binary { op, left, right } => {
            let l = evaluate(left, lookup)?;
            let r = evaluate(right, lookup)?;
            binary(op, l, r)
        }

        AstNode::If { branches, otherwise } => {
            for branch in branches {
                match evaluate(&branch.condition, lookup)? {
                    Val::Bool(true) => return evaluate(&branch.result, lookup),
                    Val::Bool(false) => continue,
                    // AN ABSENT CONDITION MAKES THE WHOLE IF ABSENT (decision 2), even
                    // when an `otherwise` exists. Falling through would answer "Small"
                    // for a group where there is no Sales at all, which is a confident
                    // bucket invented from no data. SQL's CASE would take the ELSE;
                    // we deliberately do not.
                    Val::Null => return Ok(Val::Null),
                    Val::Num(_) => {
                        return Err("an IF condition must be true/false, not a number".to_string())
                    }
                }
            }
            match otherwise {
                Some(otherwise) => evaluate(otherwise, lookup),
                None => Ok(Val::Null),
            }
        }

        AstNode::Lod { .. } => Err("level-of-detail (LOD) expressions are not supported".to_string()),
    }
}

fn binary(op: &str, left: Val, right: Val) -> Result<Val, String> {
    // Absence propagates through every operator (decision 2). ZN/IFNULL are the
    // explicit way to convert it, so nothing here invents a number.
    if matches!(left, Val::Null) || matches!(right, Val::Null) {
        return Ok(Val::Null);
    }

    match op {
        "AND" | "OR" => {
            let (Val::Bool(a), Val::Bool(b)) = (left, right) else {
                return Err(format!("{} needs true/false operands", op));
            };
            Ok(Val::Bool(if op == "AND" { a && b } else { a || b }))
        }
        "=" | "==" | "!=" | "<>" | "<" | "<=" | ">" | ">=" => {
            let (Val::Num(a), Val::Num(b)) = (left, right) else {
                return Err(format!("{} needs numeric operands", op));
            };
            Ok(Val::Bool(match op {
                "=" | "==" => a == b,
                "!=" | "<>" => a != b,
                "<" => a < b,
                "<=" => a <= b,
                ">" => a > b,
                _ => a >= b,
            }))
        }
        "+" | "-" | "*" | "/" | "%" | "^" => {
            let (Val::Num(a), Val::Num(b)) = (left, right) else {
                return Err(format!("{} needs numeric operands", op));
            };
            let value = match op {
                "+" => a + b,
                "-" => a - b,
                "*" => a * b,
                // Division by zero is ABSENT, not 0/Infinity/NaN (decision 1).
                "/" => {
                    if b == 0.0 {
                        return Ok(Val::Null);
                    }
                    a / b
                }
                "%" => {
                    if b == 0.0 {
                        return Ok(Val::Null);
                    }
                    a % b
                }
                _ => a.powf(b),
            };
            Ok(Val::Num(value))
        }
        other => Err(format!("unsupported operator '{}'", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::plan::IfBranch;

    fn field(name: &str) -> AstNode {
        AstNode::Field { name: name.to_string() }
    }

    fn agg(name: &str, f: &str) -> AstNode {
        AstNode::Call { name: name.to_string(), args: vec![field(f)] }
    }

    fn num(n: f64) -> AstNode {
        AstNode::Number { value: n }
    }

    fn binary(op: &str, l: AstNode, r: AstNode) -> AstNode {
        AstNode::Binary { op: op.to_string(), left: Box::new(l), right: Box::new(r) }
    }

    fn call(name: &str, args: Vec<AstNode>) -> AstNode {
        AstNode::Call { name: name.to_string(), args }
    }

    fn lookup_from(values: &'static [(&'static str, f64)]) -> impl Fn(&str) -> Option<f64> {
        move |name| values.iter().find(|(f, _)| *f == name).map(|(_, v)| *v)
    }

    #[test]
    fn collects_one_slot_per_field() {
        let expr = binary("/", agg("SUM", "Profit"), agg("SUM", "Sales"));
        let mut slots = Vec::new();
        collect_slots(&expr, &mut slots).expect("collect");
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].0, "Profit");
        assert_eq!(slots[1].0, "Sales");
    }

    #[test]
    fn refuses_the_things_v1_cannot_evaluate() {
        // Two aggregations of one field.
        let expr = binary("/", agg("SUM", "Sales"), agg("AVG", "Sales"));
        let mut slots = Vec::new();
        let err = collect_slots(&expr, &mut slots).expect_err("mixed aggregation");
        assert!(err.contains("one aggregation per field"), "{err}");

        // An unimplemented function names itself AND the supported set.
        let mut slots = Vec::new();
        let err = collect_slots(&call("ROUND", vec![agg("SUM", "Sales")]), &mut slots)
            .expect_err("unknown function");
        assert!(err.contains("ROUND"), "{err}");
        assert!(err.contains("ZN"), "the message should list what works: {err}");

        // A bare field is row-level, and there are no rows here.
        let mut slots = Vec::new();
        let err = collect_slots(&field("Sales"), &mut slots).expect_err("bare field");
        assert!(err.contains("row-level"), "{err}");

        // LOD is a second query, not an expression.
        let mut slots = Vec::new();
        let lod = AstNode::Lod {
            spec: crate::engine::plan::LodSpec::Fixed,
            dimensions: vec!["Region".to_string()],
            expression: Box::new(agg("SUM", "Sales")),
        };
        let err = collect_slots(&lod, &mut slots).expect_err("lod");
        assert!(err.contains("level-of-detail"), "{err}");
    }

    #[test]
    fn arithmetic_and_absence() {
        let values: &[(&str, f64)] = &[("Profit", 25.0), ("Sales", 100.0)];
        let lookup = lookup_from(values);

        let margin = binary("/", agg("SUM", "Profit"), agg("SUM", "Sales"));
        assert_eq!(evaluate(&margin, &lookup), Ok(Val::Num(0.25)));

        // Division by zero is ABSENT, not 0 and not Infinity (decision 1).
        let zero: &[(&str, f64)] = &[("Profit", 25.0), ("Sales", 0.0)];
        assert_eq!(evaluate(&margin, &lookup_from(zero)), Ok(Val::Null));

        // ZN has to WRAP THE DIVISION to rescue it — wrapping only the numerator leaves
        // the division by zero to produce absence, which is the point of decision 1.
        let rescued = call("ZN", vec![margin.clone()]);
        assert_eq!(evaluate(&rescued, &lookup_from(zero)), Ok(Val::Num(0.0)));

        let numerator_only = binary("/", call("ZN", vec![agg("SUM", "Profit")]), agg("SUM", "Sales"));
        assert_eq!(
            evaluate(&numerator_only, &lookup_from(zero)),
            Ok(Val::Null),
            "ZN on the numerator does not make the quotient zero"
        );
    }

    #[test]
    fn an_absent_operand_propagates() {
        // Sales is present, Profit is not: the margin is UNKNOWN, not 0% (decision 2).
        let only_sales: &[(&str, f64)] = &[("Sales", 100.0)];
        let margin = binary("/", agg("SUM", "Profit"), agg("SUM", "Sales"));
        assert_eq!(evaluate(&margin, &lookup_from(only_sales)), Ok(Val::Null));

        // ... unless the caller asks for 0.
        let rescued = binary("/", call("ZN", vec![agg("SUM", "Profit")]), agg("SUM", "Sales"));
        assert_eq!(evaluate(&rescued, &lookup_from(only_sales)), Ok(Val::Num(0.0)));

        // IFNULL on the numerator likewise.
        let ifnull = call("IFNULL", vec![agg("SUM", "Profit"), num(0.0)]);
        assert_eq!(evaluate(&ifnull, &lookup_from(only_sales)), Ok(Val::Num(0.0)));
    }

    #[test]
    fn if_branches_pick_the_first_true_condition() {
        let expr = AstNode::If {
            branches: vec![
                IfBranch {
                    condition: Box::new(binary(">", agg("SUM", "Sales"), num(100.0))),
                    result: Box::new(num(1.0)),
                },
                IfBranch {
                    condition: Box::new(binary(">", agg("SUM", "Sales"), num(0.0))),
                    result: Box::new(num(2.0)),
                },
            ],
            otherwise: Some(Box::new(num(3.0))),
        };
        let lookup = lookup_from(&[("Sales", 500.0)]);
        assert_eq!(evaluate(&expr, &lookup), Ok(Val::Num(1.0)));
        let lookup = lookup_from(&[("Sales", 50.0)]);
        assert_eq!(evaluate(&expr, &lookup), Ok(Val::Num(2.0)));
        let lookup = lookup_from(&[("Sales", 0.0)]);
        assert_eq!(evaluate(&expr, &lookup), Ok(Val::Num(3.0)));
        // No cells at all: no branch matches, so the result is absent, not 3.
        let absent = lookup_from(&[]);
        assert_eq!(evaluate(&expr, &absent), Ok(Val::Null));
    }

    #[test]
    fn a_boolean_result_is_not_a_measure() {
        let expr = binary(">", agg("SUM", "Sales"), num(0.0));
        let value = evaluate(&expr, &lookup_from(&[("Sales", 1.0)])).expect("evaluates");
        assert!(value.into_number().is_err(), "a comparison is not a number column");
    }
}
