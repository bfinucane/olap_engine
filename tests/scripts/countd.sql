-- EXACT DISTINCT COUNT: COUNTD counts DISTINCT BASE leaf VALUES, not cells.
--
-- Two cells in Europe hold the SAME value (100): COUNT = 2 but COUNTD = 1. Germany
-- holds a different value, so a grand total over all three is COUNTD 2.
--
-- A FRACTIONAL weight in a SECOND hierarchy (`.rollup Weighted Half England 0.5`) pins
-- the weight rule: like AVG/MIN/MAX, COUNTD folds the BASE leaf value and IGNORES
-- rollup weights, so Half's SUM is 50 while its COUNTD is still 1. (It has to be a
-- separate hierarchy: a member has exactly one parent per hierarchy, so weighting
-- England inside the default one would move it out of Europe.)
--
-- Cost exists only for England, so a group containing England has one Cost value and a
-- group without it has none at all -- absent, never zero (the engine prints '-').
--
-- An ATTRIBUTE table holds TEXT. COUNTD compares strings EXACTLY and case-sensitively:
-- "Dan" and "dan" are two different values, while the repeated "Ava" is one.

CREATE TABLE Sales (Geography STRING, Measure MEASURE)

INSERT INTO Sales VALUES ('England', 'Sales', 100)
INSERT INTO Sales VALUES ('France', 'Sales', 100)
INSERT INTO Sales VALUES ('Germany', 'Sales', 200)
INSERT INTO Sales VALUES ('England', 'Cost', 40)

.rollup Geography Europe England 1.0
.rollup Geography Europe France 1.0

.create_hierarchy Geography Weighted
.rollup Weighted Half England 0.5

-- COUNT counts CELLS; COUNTD counts distinct VALUES. Same two cells, both 100.
SELECT COUNT(Sales) FROM Sales WHERE Geography = 'Europe'
SELECT COUNTD(Sales) FROM Sales WHERE Geography = 'Europe'
SELECT SUM(Sales) FROM Sales WHERE Geography = 'Europe'

-- A weight halves SUM but changes COUNTD not at all: the base value is still 100.
SELECT SUM(Sales) FROM Sales WHERE Weighted = 'Half'
SELECT COUNTD(Sales) FROM Sales WHERE Weighted = 'Half'

-- Absent cells are not values: Europe has one Cost cell (England's 40).
SELECT COUNT(Cost) FROM Sales WHERE Geography = 'Europe'
SELECT COUNTD(Cost) FROM Sales WHERE Geography = 'Europe'

-- Germany has no Cost cell at all: the absent marker, never 0.
SELECT COUNTD(Cost) FROM Sales WHERE Geography = 'Germany'

-- The standard SQL spelling COUNT(DISTINCT ...) IS COUNTD, not plain COUNT: same 1.
SELECT COUNT(DISTINCT Sales) FROM Sales WHERE Geography = 'Europe'

-- TEXT on an ATTRIBUTE table: COUNTD compares strings EXACTLY and case-sensitively.
-- `WHERE Store IN (...)` keeps every store of the East group: SQL defaults any
-- unfiltered dimension to its first member, which would hide the other three cells.
CREATE ATTRIBUTE TABLE RegionAttr (Region STRING, Store STRING, Attribute MEASURE)
INSERT INTO RegionAttr VALUES ('East', 'S1', 'Manager', 'Ava')
INSERT INTO RegionAttr VALUES ('East', 'S2', 'Manager', 'Ava')
INSERT INTO RegionAttr VALUES ('East', 'S3', 'Manager', 'Dan')
INSERT INTO RegionAttr VALUES ('East', 'S4', 'Manager', 'dan')
INSERT INTO RegionAttr VALUES ('West', 'S1', 'Manager', 'Dan')
-- East: 'Ava' twice -> one value; 'Dan' and 'dan' -> two more. COUNTD = 3, not 4.
-- West: one value. ('COUNT' cannot stand in here: a text cell replaces the numeric
-- accumulator, so COUNT over text is not a cell count - a pre-existing behaviour.)
SELECT Region, COUNTD(Manager) FROM RegionAttr WHERE Store IN ('S1', 'S2', 'S3', 'S4')
