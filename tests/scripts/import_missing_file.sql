-- Regression test: importing from a missing CSV must produce a clean error,
-- not a panic (previously .import_pc/.import_lvl unwrapped the file open).
CREATE TABLE T (A STRING, B STRING, V MEASURE)

.import_pc tests/scripts/does_not_exist.csv Geography
.import_lvl tests/scripts/also_missing.csv Calendar
.import tests/scripts/no_such_data.csv T
