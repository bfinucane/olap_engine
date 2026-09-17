-- Member removal tests.
--   1. .detach        - remove a parent/child relation (member stays).
--   2. .delete_member - delete a member entirely and purge its data from
--                       EVERY cube that shares the dimension.
-- Deleting an aggregate makes its children pop to the top level.

CREATE TABLE Sales (Region STRING, Product STRING)
.rollup Region Europe France 1.0
.rollup Region Europe Germany 1.0


.rollup Region "North America" USA 1.0
.rollup Region "North America" Canada 1.0

INSERT INTO Sales VALUES ('France', 'Widget', 10)
INSERT INTO Sales VALUES ('Germany', 'Widget', 20)
INSERT INTO Sales VALUES ('USA', 'Widget', 30)
INSERT INTO Sales VALUES ('Canada', 'Widget', 40)

-- Baseline: Europe = 30, All regions via North America = 70.
SELECT Region, value FROM Sales
SELECT value FROM Sales WHERE Region = 'Europe'

-- A second cube sharing the SAME Region dimension.
CREATE TABLE Plan (Region STRING, Product STRING)
INSERT INTO Plan VALUES ('France', 'Widget', 100)
INSERT INTO Plan VALUES ('Germany', 'Widget', 200)
INSERT INTO Plan VALUES ('USA', 'Widget', 300)
SELECT value FROM Plan WHERE Region = 'Europe'

-- ============================================================
-- 1. Detach France from Europe.
--    Europe becomes a parent of only Germany (France stays a member).
-- ============================================================
.detach Region Europe France
.tree Region Europe
SELECT value FROM Sales WHERE Region = 'Europe'

-- Re-attach, then delete the whole Europe member.
.rollup Region Europe France 1.0

-- ============================================================
-- 2. Delete the Europe member.
--    France and Germany pop to the top level (they remain leaves).
--    All data in 'Sales' AND 'Plan' with Region = Europe is purged,
--    but France/Germany cell data survives as top-level leaves.
-- ============================================================
.delete_member Region Europe
SELECT Region, value FROM Sales
SELECT Region, value FROM Plan

-- Europe is gone as a member entirely.
SELECT value FROM Sales WHERE Region = 'Europe'

-- The remaining members still consolidate normally under a fresh parent.
.rollup Region Europe2 France 1.0
.rollup Region Europe2 Germany 1.0
SELECT value FROM Sales WHERE Region = 'Europe2'
