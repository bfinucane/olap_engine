-- Tests the CSV import paths, which had no coverage before.
-- 1) .import     : bulk data load (columns == cube dims, last col = value)
-- 2) .import_pc  : parent/child/weight hierarchy from CSV
-- 3) .import_lvl : level-based ("ragged") hierarchy from CSV

CREATE TABLE Financials (Calendar STRING, Geography STRING, Scenario STRING, Measure MEASURE)

-- 1) Bulk-load base rows straight from CSV.
.import tests/scripts/import_data.csv Financials

-- 2) Build the geography rollup from a parent/child/weight CSV.
.import_pc tests/scripts/import_pc.csv Geography

-- 3) Build a calendar rollup from a level-based CSV.
.import_lvl tests/scripts/import_lvl_calendar.csv Calendar

-- ---- Verify the .import data landed correctly (expect three Jan/Feb cells) ----
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'France' AND Scenario = 'Actuals'
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'Germany' AND Scenario = 'Actuals'

-- ---- Verify .import_pc built the geography tree: Europe = France + Germany ----
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'Europe' AND Scenario = 'Actuals'

-- ---- Verify a space-containing member from .import_pc rolls up ----
SELECT Sales FROM Financials WHERE Calendar = 'Jan' AND Geography = 'All' AND Scenario = 'Actuals'

-- ---- Verify .import_lvl built the calendar tree: Q1 = Jan + Feb = 100 + 110 ----
SELECT Sales FROM Financials WHERE Calendar = 'Q1' AND Geography = 'France' AND Scenario = 'Actuals'

