-- Data allocation ("splashing") test.
-- Writing to a CONSOLIDATED member distributes the value to its leaf
-- descendants instead of storing a value at the aggregate node.
-- The aggregate read-back must equal the splashed total exactly.

CREATE TABLE Splash (Geography STRING, Scenario STRING, Measure MEASURE)

-- Geography:  England, France, Germany -> Europe
--             USA, Canada              -> North America
--             Europe, North America    -> All
.rollup Geography Europe England 1.0
.rollup Geography Europe France 1.0
.rollup Geography Europe Germany 1.0
.rollup Geography "North America" USA 1.0
.rollup Geography "North America" Canada 1.0
.rollup Geography All Europe 1.0
.rollup Geography All "North America" 1.0

-- Scenario:  Variant = Actuals - Plan   (a NEGATIVE-weight consolidation)
.rollup Scenario Variant Actuals 1.0
.rollup Scenario Variant Plan -1.0

-- ============================================================
-- 1. Even split across Europe (England/France/Germany) = 1:1:1.
--    Splash 300 -> 100 / 100 / 100, and Europe reads back 300.
--    Coordinates are given in dimension order: Geography, Scenario, Measure.
-- ============================================================
.splash Splash 300 Europe Actuals Sales
SELECT Geography, Sales FROM Splash
SELECT Sales FROM Splash WHERE Geography = 'Europe' AND Scenario = 'Actuals'

-- ============================================================
-- 2. Deep roll: splash at "All" spreads across every leaf under it,
--    weighted so All reads back the exact total.
-- ============================================================
.splash Splash 1000 All Actuals Sales
SELECT Sales FROM Splash WHERE Geography = 'All' AND Scenario = 'Actuals'

-- ============================================================
-- 3. ADD mode: spread 90 on TOP of existing leaves for North America.
--    USA is seeded with 10 first, so the two leaves are uneven.
-- ============================================================
INSERT INTO Splash VALUES ('USA', 'Plan', 'Sales', 10)
.splash ADD Splash 90 "North America" Plan Sales
SELECT Geography, Sales FROM Splash WHERE Geography IN ('USA', 'Canada') AND Scenario = 'Plan'
SELECT Sales FROM Splash WHERE Geography = 'North America' AND Scenario = 'Plan'

-- ============================================================
-- 4. Negative-weight allocation (Variant = Actuals - Plan).
--    The new cube REUSES the shared Geography / Scenario dimensions
--    defined above - conformed dimensions are the point of the engine.
--    Splashing 300 into Variant sets Actuals = +150 and Plan = -150 per
--    leaf, so (Actuals - Plan) = 300.
-- ============================================================
CREATE TABLE Neg (Scenario STRING, Geography STRING, Measure MEASURE)
.splash Neg 300 Variant Europe Sales
SELECT Scenario, Sales FROM Neg WHERE Geography = 'Europe'
SELECT Sales FROM Neg WHERE Scenario = 'Variant' AND Geography = 'Europe'

