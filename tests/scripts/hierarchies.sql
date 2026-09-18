-- MULTI-HIERARCHY DIMENSIONS
--
-- A dimension owns one or more HIERARCHIES over the SAME base (leaf) members.
-- Every dimension is born with a DEFAULT hierarchy named after the dimension,
-- so single-hierarchy scripts are unchanged. A DIMENSION NAME is a dynamic
-- ALIAS for that dimension's default hierarchy; a HIERARCHY name stands for
-- itself. Hierarchy names are unique across the database.

CREATE TABLE Sales (Geography STRING, Scenario STRING, Measure MEASURE)

-- Write data at leaves. Leaves are shared: the SAME leaf is reachable through
-- every hierarchy of its dimension. Columns are (Geography, Scenario, Measure)
-- plus the trailing value, so each row has FOUR values.
INSERT INTO Sales VALUES ('Paris', 'Actuals', 'Sales', 100)
INSERT INTO Sales VALUES ('Lyon', 'Actuals', 'Sales', 50)
INSERT INTO Sales VALUES ('Berlin', 'Actuals', 'Sales', 70)
INSERT INTO Sales VALUES ('Rome', 'Actuals', 'Sales', 30)

-- 1. The dimension starts with exactly one (default) hierarchy, named after it.
.dimensions

-- 2. Create a SECOND hierarchy: 'Geography' (by market) vs 'Ops' (by warehouse).
.create_hierarchy Geography Ops

-- 3. Build the DEFAULT geography tree (referenced by DIMENSION name -> alias).
.rollup Geography Europe Paris 1.0
.rollup Geography Europe Lyon 1.0
.rollup Geography Europe Berlin 1.0
.rollup Geography Southern Rome 1.0

-- 4. Build an INDEPENDENT tree in the 'Ops' hierarchy over the SAME leaves.
--    Note the deliberate cross-cut: Paris and Berlin share an ops parent even
--    though they sit under different market parents.
.rollup Ops Warehouse_A Paris 1.0
.rollup Ops Warehouse_A Berlin 1.0
.rollup Ops Warehouse_B Lyon 1.0
.rollup Ops Warehouse_B Rome 1.0

-- 5. Aggregate through the DEFAULT hierarchy (dimension name = its default
--    hierarchy). Europe groups Paris + Lyon + Berlin = 220. `Sales` is a MEASURE
--    member used directly as a SELECT column (no `value` keyword needed).
SELECT Geography, Sales FROM Sales WHERE Geography = 'Europe'
-- 6. Aggregate the SAME leaves through the SECOND hierarchy. Warehouse_A groups
--    Paris + Berlin = 170 (a different grouping over identical leaf data).
SELECT Ops, Sales FROM Sales WHERE Ops = 'Warehouse_A'

-- 7. Build a top level in each hierarchy, then read it back. Both total 250, but
--    they group the leaves differently (see the two trees below).
.rollup Geography All_Geo Europe 1.0
.rollup Geography All_Geo Southern 1.0
.rollup Ops All_Ops Warehouse_A 1.0
.rollup Ops All_Ops Warehouse_B 1.0
SELECT Geography, Sales FROM Sales WHERE Geography = 'All_Geo'
SELECT Ops, Sales FROM Sales WHERE Ops = 'All_Ops'

-- 7b. Axis selection WITHOUT a hierarchy qualifier uses the dimension's default
--     hierarchy, showing each leaf in creation order.
SELECT Geography, Sales FROM Sales

-- 8. .tree shows each hierarchy's own structure.
.tree Geography All_Geo
.tree Ops All_Ops

-- 9. .order is hierarchy-scoped: reorder one without touching the other.
.order Geography Southern front
.order Geography
.order Ops

-- 10. A hierarchy name must be unique across the database.
.create_hierarchy Scenario Geography

-- 11. Creating a hierarchy that already exists in the same dimension is an error.
.create_hierarchy Geography Ops

