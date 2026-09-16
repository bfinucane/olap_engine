-- Ragged / messy level-based hierarchy import (financial-accounting style).
-- Paths have DIFFERENT lengths and blanks appear at the bottom:
--   All > Europe > France > Paris/Lyon   (deep)
--   All > Asia   > Japan                 (medium, no city)
--   All > Africa                          (shallow, country-level leaf)
--   All                                    (bare rollup row)
-- Verifies that blanks are skipped, so each value links to the NEXT non-empty
-- value to its right (e.g. All>Asia>Japan even when the city column is blank).

CREATE TABLE Financials (Geography STRING, Measure MEASURE)

.import_lvl tests/scripts/import_lvl_ragged.csv Geography

-- Write values at a deep leaf (Paris/Lyon) and a shallow leaf (Africa).
-- Insert order: Geography, Measure member, Value.
INSERT INTO Financials VALUES ('Paris', 'Sales', 10)
INSERT INTO Financials VALUES ('Lyon', 'Sales', 5)
INSERT INTO Financials VALUES ('Africa', 'Sales', 100)

-- Leaf read.
SELECT Sales FROM Financials WHERE Geography = 'Paris'

-- France = Paris + Lyon = 15.
SELECT Sales FROM Financials WHERE Geography = 'France'

-- Europe = France = 15.
SELECT Sales FROM Financials WHERE Geography = 'Europe'

-- All = Paris + Lyon + Africa = 115 (Japan has no data).
SELECT Sales FROM Financials WHERE Geography = 'All'

-- Messy level: SubRegion is blank on the Iceland row, so Iceland hangs
-- directly off All and Reykjavik hangs off Iceland.
INSERT INTO Financials VALUES ('Reykjavik', 'Sales', 7)
SELECT Sales FROM Financials WHERE Geography = 'Iceland'
SELECT Sales FROM Financials WHERE Geography = 'All'

