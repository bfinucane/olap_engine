-- End-to-end aggregation test.
-- Exercises: weighted consolidation (Variant = Actuals - Plan),
-- multi-level rollups, and ragged two-branch geography.
-- NOTE: the script runner executes one command per line.

-- Dimensions come to life as we reference them in CREATE TABLE.
CREATE TABLE Financials (Calendar STRING, Geography STRING, Scenario STRING, Measure MEASURE)
-- Calendar hierarchy:  Months -> Quarter -> Total
.rollup Calendar Q1 Jan 1.0
.rollup Calendar Q1 Feb 1.0
.rollup Calendar Q1 Mar 1.0
.rollup Calendar Q2 Apr 1.0
.rollup Calendar Q2 May 1.0
.rollup Calendar Q2 Jun 1.0
.rollup Calendar Total Q1 1.0
.rollup Calendar Total Q2 1.0

-- Geography hierarchy:
--   England, France, Germany   -> Europe
--   USA, Canada, Mexico        -> North America
--   Europe, North America      -> All
.rollup Geography Europe England 1.0
.rollup Geography Europe France 1.0
.rollup Geography Europe Germany 1.0
.rollup Geography "North America" USA 1.0
.rollup Geography "North America" Canada 1.0
.rollup Geography "North America" Mexico 1.0
.rollup Geography All Europe 1.0
.rollup Geography All "North America" 1.0

-- Scenario hierarchy:  Variant = Actuals - Plan
.rollup Scenario Variant Actuals 1.0
.rollup Scenario Variant Plan -1.0

-- Load a few base values (leaf cells only).
INSERT INTO Financials VALUES ('Jan', 'France', 'Actuals', 'Sales', 100)
INSERT INTO Financials VALUES ('Jan', 'France', 'Actuals', 'Costs', 40)
INSERT INTO Financials VALUES ('Jan', 'France', 'Plan', 'Sales', 90)
INSERT INTO Financials VALUES ('Jan', 'France', 'Plan', 'Costs', 45)

INSERT INTO Financials VALUES ('Jan', 'Germany', 'Actuals', 'Sales', 200)
INSERT INTO Financials VALUES ('Jan', 'Germany', 'Actuals', 'Costs', 80)
INSERT INTO Financials VALUES ('Jan', 'Germany', 'Plan', 'Sales', 210)
INSERT INTO Financials VALUES ('Jan', 'Germany', 'Plan', 'Costs', 70)

INSERT INTO Financials VALUES ('Jan', 'USA', 'Actuals', 'Sales', 500)
INSERT INTO Financials VALUES ('Jan', 'USA', 'Actuals', 'Costs', 300)
INSERT INTO Financials VALUES ('Jan', 'USA', 'Plan', 'Sales', 450)
INSERT INTO Financials VALUES ('Jan', 'USA', 'Plan', 'Costs', 280)

INSERT INTO Financials VALUES ('Feb', 'France', 'Actuals', 'Sales', 110)
INSERT INTO Financials VALUES ('Feb', 'France', 'Actuals', 'Costs', 50)

-- ============================================================
-- QUERIES: verify the sums at each level of aggregation.
-- ============================================================

-- 1. Leaf sanity: one fully-specified cell = 100.
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'France' AND Scenario = 'Actuals'

-- 2. Europe (France + Germany) Jan Actuals Sales = 100 + 200 = 300.
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'Europe' AND Scenario = 'Actuals'

-- 3. North America Jan Plan Sales = 450 (only USA has data).
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'North America' AND Scenario = 'Plan'

-- 4. All Jan Actuals Sales = 100 + 200 + 500 = 800.
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'All' AND Scenario = 'Actuals'

-- 5. Variant (Actuals - Plan) Europe Jan Sales = 300 - (90 + 210) = 0.
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'Europe' AND Scenario = 'Variant'

-- 6. Q1 rollup (Jan + Feb) France Actuals Sales = 100 + 110 = 210.
SELECT Sales FROM Financials WHERE Calendar = 'Q1' AND Geography = 'France' AND Scenario = 'Actuals'

-- 7. Measure pivot: Sales and Costs together for Germany Jan Actuals.
SELECT Geography, Sales, Costs FROM Financials WHERE Calendar = 'Jan' AND Geography = 'Germany' AND Scenario = 'Actuals'

