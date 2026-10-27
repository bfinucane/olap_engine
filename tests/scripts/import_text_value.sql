-- A TEXT value in a NUMERIC cube is refused, with the same rule as the write API:
-- a string cell folds as "last value wins" and would silently replace a group's total.
-- Text belongs in an attribute table.
CREATE TABLE Numeric (Region STRING, Amount MEASURE)
.import tests/scripts/import_text_value.csv Numeric
-- The string-specific importer must refuse the same cube: it writes text unconditionally.
.import_attr tests/scripts/import_text_value.csv Numeric