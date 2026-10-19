-- NON-ADDITIVE AGGREGATIONS: SUM, AVG, COUNT, MIN, MAX.
--
-- Two cells per group, so the aggregations actually differ:
--   Europe: England 100, France 50
--     SUM 150   AVG 75   COUNT 2   MIN 50   MAX 100
--
-- Then a FRACTIONAL weight in a SECOND hierarchy (`.rollup Weighted Half England 0.5`)
-- pins the decision recorded in AGGREGATIONS-DESIGN.md §4: the non-additive
-- aggregations fold the BASE leaf value and IGNORE rollup weights, so Half's SUM is 50
-- but its AVG is still 100. A weight says how much a leaf contributes to a rollup, not
-- how much it should count in a mean. (It has to be a separate hierarchy: a member has
-- exactly one parent per hierarchy, so weighting England inside the default one would
-- move it out of Europe.)
--
-- Finally an ABSENT measure: Cost exists only for England, so a group without England
-- has no cost at all -- not a zero.

CREATE TABLE Sales (Geography STRING, Measure MEASURE)

INSERT INTO Sales VALUES ('England', 'Sales', 100)
INSERT INTO Sales VALUES ('France', 'Sales', 50)
INSERT INTO Sales VALUES ('Germany', 'Sales', 200)
INSERT INTO Sales VALUES ('England', 'Cost', 40)

.rollup Geography Europe England 1.0
.rollup Geography Europe France 1.0

.create_hierarchy Geography Weighted
.rollup Weighted Half England 0.5

-- SUM, AVG, COUNT, MIN, MAX over the same two cells.
SELECT SUM(Sales) FROM Sales WHERE Geography = 'Europe'
SELECT AVG(Sales) FROM Sales WHERE Geography = 'Europe'
SELECT COUNT(Sales) FROM Sales WHERE Geography = 'Europe'
SELECT MIN(Sales) FROM Sales WHERE Geography = 'Europe'
SELECT MAX(Sales) FROM Sales WHERE Geography = 'Europe'

-- A fractionally weighted parent: SUM halves, the non-additive ones do not.
SELECT SUM(Sales) FROM Sales WHERE Weighted = 'Half'
SELECT AVG(Sales) FROM Sales WHERE Weighted = 'Half'
SELECT COUNT(Sales) FROM Sales WHERE Weighted = 'Half'

-- A measure that exists for only one of the group's cells: France has no Cost.
SELECT SUM(Cost) FROM Sales WHERE Geography = 'Europe'
SELECT AVG(Cost) FROM Sales WHERE Geography = 'Europe'
SELECT COUNT(Cost) FROM Sales WHERE Geography = 'Europe'

-- A group with no cell at all for the measure: absent, never zero.
SELECT AVG(Cost) FROM Sales WHERE Geography = 'Germany'
SELECT COUNT(Cost) FROM Sales WHERE Geography = 'Germany'

-- An aggregation the engine cannot compute is refused with a clear message, not a
-- confusing "column not found" and never a silent SUM.
SELECT MEDIAN(Sales) FROM Sales WHERE Geography = 'Europe'
