-- Import summary and measureless OLAP form.
-- Every dimension is a column; the value is the trailing extra column, so a
-- well-formed row has exactly dim_count + 1 fields. A 'Measure' dimension is
-- just another coordinate (classic pre-MDX form), NOT a magic value slot.
CREATE TABLE Sales (Region STRING, Product STRING, Measure MEASURE)

-- Well-formed: 3 coordinate columns + 1 value column = 4 fields.
INSERT INTO Sales VALUES ('North', 'Widget', 'Sales', 100)
INSERT INTO Sales VALUES ('South', 'Widget', 'Sales', 200)
SELECT Region, Sales FROM Sales
