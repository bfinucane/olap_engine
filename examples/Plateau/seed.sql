-- examples/Plateau/seed.sql
--
-- Builds a demo catalog from the Plateau fixtures in this directory and saves
-- it to `database.bin`, so `serve` has real data to answer Plateau with.
--
-- RUN (from the repo root):
--     cargo run --bin olap_engine -- examples/Plateau/seed.sql
--     cargo run --bin serve
--
-- WHY A SCRIPT (not Rust): every load step below uses the engine's OWN SQL and
-- meta-commands (.import / .import_attr / .import_pc / .import_lvl / .save), so
-- there is no second copy of the import logic to drift out of sync.
--
-- The batch runner starts from an EMPTY catalog and does NOT load database.bin;
-- only `serve` does. The trailing `.save` is what produces the file serve reads.

-- --------------------------------------------------------------------------
-- 1. Sales: the dense, hand-checkable cube (3 dims; the fixture's final
--    `Sales` column becomes the value CELL, so the cube needs no measure dim).
--    Fixture columns: Region,Segment,Category,Sales  (4 cols = 3 dims + value).
--    Grand total across all 36 rows: 26,763,427.43
-- --------------------------------------------------------------------------
CREATE TABLE Sales (Region STRING, Segment STRING, Category STRING)
.import examples/Plateau/cube-region-segment-category.csv Sales

-- --------------------------------------------------------------------------
-- 2. Pivot: Region, Category and a literal `Measure` DIMENSION.
--    Fixture columns: Region,Category,Measure,Value (4 cols = 3 dims + value).
-- --------------------------------------------------------------------------
CREATE TABLE Pivot (Region STRING, Category STRING, Measure STRING)
.import examples/Plateau/measure-pivot.csv Pivot

-- --------------------------------------------------------------------------
-- 3. RegionAttr: a clean ATTRIBUTE table (one row per Region) for JOIN demos.
--    This is the string-attribute shape, loaded with `.import_attr`:
--      fixture columns: Region,Attribute,Value
--      (keys..., attribute-name, string-value)
--    Shares the `Region` conformed dimension with Sales/Pivot, so
--    `SELECT ... FROM Sales JOIN RegionAttr USING (Region)` enriches Sales.
-- --------------------------------------------------------------------------
CREATE ATTRIBUTE TABLE RegionAttr (Region STRING, Attribute MEASURE)
.import_attr examples/Plateau/region-attributes.csv RegionAttr

-- --------------------------------------------------------------------------
-- 4. Attributes: string-valued attribute cube, Region+Category -> Product.
--    `attribute-strings.csv` is Region,Category,Product (3 columns). `.import`
--    reads (dims) key columns + 1 trailing value column, so a TWO-dimension
--    cube fits exactly: the product name lands as the string CELL.
--    (Use `RegionAttr` above for a JOIN with a NAMED attribute; this cube is
--     here so Plateau has a string-cell shape to browse/describe/sample.)
-- --------------------------------------------------------------------------
CREATE ATTRIBUTE TABLE Attributes (Region STRING, Category STRING)
.import examples/Plateau/attribute-strings.csv Attributes

-- --------------------------------------------------------------------------





-- 5. Geography: a STAND-ALONE dimension (with its own cube) that carries the
--    four-level city hierarchy. It is deliberately NOT built on `Region`.
--
--    WHY: a member of a dimension is either a base (leaf) member OR an
--    aggregate, never both, and the leaf bank is per-dimension. The tree's
--    level-2 nodes are named `Central`/`East`/... just like Sales' `Region`
--    leaves; building the tree ON `Region` would promote those base members to
--    aggregates and evict them from `Region` entirely (which the engine warns
--    about, and which would strand Sales' stored facts). Giving the tree its
--    own dimension keeps Sales' flat `Region` intact.
--
--    `Geography` carries the four-level city tree; the tree is created as a
--    SECOND hierarchy alongside the dimension's default one, then fed the
--    existing level fixture with `.import_lvl`.
-- --------------------------------------------------------------------------
CREATE TABLE Geography (Geo STRING, Measure MEASURE)
CREATE HIERARCHY Cities IN Geo
.import_lvl examples/Plateau/hierarchy-levels.csv Cities
-- --------------------------------------------------------------------------
-- 6. ProductClass: the same idea for the Category tree, on its OWN dimension.
--    (The source fixture is the level form, so `.import_lvl` again.)
-- --------------------------------------------------------------------------
CREATE TABLE ProductClass (Product STRING, Measure MEASURE)
CREATE HIERARCHY Product_Class IN Product
.import_lvl examples/Plateau/hierarchy-product.csv Product_Class




-- --------------------------------------------------------------------------

-- 7. CostCentre: the mixed Total tree, kept in its OWN cube/dimension because
--    its members span what are, elsewhere, two different dimensions. Loading it
--    in isolation preserves its opposite-sign consolidation weights faithfully.
-- --------------------------------------------------------------------------
CREATE TABLE CostCentre (Account STRING, Measure MEASURE)
.import_pc examples/Plateau/hierarchy-parent-child.csv Account

-- --------------------------------------------------------------------------
-- Sanity checks (printed during the seed run; failure sets the exit code).
-- --------------------------------------------------------------------------
SELECT Region, value FROM Sales
SELECT Region, value FROM Pivot WHERE Region = 'East' AND Category = 'Furniture' AND Measure = 'Sales'
SELECT Region, Attribute FROM RegionAttr
SELECT Region, Segment, value FROM Sales WHERE Region = 'Central'
.tree Cities All

-- --------------------------------------------------------------------------
-- Persist. This is what `serve` loads on startup.
-- --------------------------------------------------------------------------
.save
