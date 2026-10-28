-- SQL ORDER BY: the SQL text front end sorts before it pages.
--
-- The wire/plan layer already honoured `orderBy`. The SQL path read only
-- LIMIT/OFFSET and SILENTLY DROPPED the ORDER BY clause: the rows came back in
-- display order with no error, the plausible-wrong-answer failure mode this
-- engine refuses elsewhere. It now parses the key and applies it.
--
-- Semantics are the plan layer's (engine::plan::apply_order_and_limit), so the
-- two front doors cannot disagree: a STABLE sort, numeric when both cells parse
-- as a number and lexicographic otherwise, ties kept in display order. SQL's
-- order of operations applies: ORDER BY sorts the whole result, THEN LIMIT/OFFSET
-- pages it.
--
-- Refused loudly rather than ignored: an unknown column, a non-1-based position,
-- NULLS FIRST/LAST, and any expression other than a column/position. See
-- `order_by_unknown_column.sql` and the unit tests in engine::sql.

CREATE TABLE T (Region STRING, Product STRING, M MEASURE)

INSERT INTO T VALUES ('East', 'A', 'Sales', 9)
INSERT INTO T VALUES ('East', 'B', 'Sales', 10)
INSERT INTO T VALUES ('West', 'A', 'Sales', 9)
INSERT INTO T VALUES ('West', 'B', 'Sales', 12)

-- Display order: insertion order, for comparison with the sorted queries below.
SELECT Region, Product, Sales FROM T

-- Numeric, not lexicographic: 9 sorts before 10 (text order would put "10" first).
-- The two 9s are a tie, so display order decides (East/A before West/A).
SELECT Region, Product, Sales FROM T ORDER BY Sales

-- Descending.
SELECT Region, Product, Sales FROM T ORDER BY Sales DESC

-- A text column.
SELECT Region, Product, Sales FROM T ORDER BY Region, Product

-- ORDER BY then LIMIT: the sort covers the WHOLE result, so this is the top two
-- by Sales, not the first two rows re-sorted.
SELECT Region, Product, Sales FROM T ORDER BY Sales DESC LIMIT 2

-- A 1-based output-column position is accepted, like standard SQL.
SELECT Region, Product, Sales FROM T ORDER BY 3 DESC

-- Several keys: Sales DESC, then Region ASC to break the 9/9 tie.
SELECT Region, Product, Sales FROM T ORDER BY Sales DESC, Region
