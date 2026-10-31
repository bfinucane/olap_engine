-- examples/superstore-v2.sql
--
-- MODEL v2 — the RAW Superstore line items (contract handoff 012, owner ruling D19).
--
-- WHY a separate seed (not an edit of seed.sql): model v1 is kept reproducible as history;
-- v2 replaces it as the shared model only when the user promotes the release.
--
-- SOURCE: examples/Plateau/superstore-raw.csv — 5,560 rows, 21 columns, one row per order
-- line item. Four columns are numeric MEASURES (Sales, Quantity, Discount, Profit); every
-- other column is a dimension, loaded in header order. `.import_measures` creates the cube
-- `Orders` from the CSV header and stores one cell per measure per row, so every measure is
-- queryable under its own name through the normal measure surface.
--
-- RUN (from the engine repo root):
--     target\debug\olap_engine.exe --db <scratch>.bin examples\superstore-v2.sql
--
-- The trailing `.save` is what produces the database file `serve` loads.
.import_measures examples/Plateau/superstore-raw.csv Orders Sales,Profit,Quantity,Discount examples/superstore-v2-renames.txt
.save