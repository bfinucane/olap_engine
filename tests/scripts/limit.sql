-- LIMIT and OFFSET on the SQL front end.
--
-- The engine materialises the grouped result and then pages it, so LIMIT counts
-- OUTPUT rows exactly as the user sees them (after any joined attribute columns are
-- appended). OFFSET skips before LIMIT applies.
--
-- `LIMIT ALL` means no limit. A non-literal (`LIMIT 'two'`, `LIMIT 1+1`) is a clean
-- error rather than a silently ignored clause -- see the unit tests in engine::sql.

CREATE TABLE T (Region STRING, Product STRING, M MEASURE)

INSERT INTO T VALUES ('East', 'A', 'Sales', 10)
INSERT INTO T VALUES ('East', 'B', 'Sales', 20)
INSERT INTO T VALUES ('West', 'A', 'Sales', 30)
INSERT INTO T VALUES ('West', 'B', 'Sales', 40)

-- All four rows.
SELECT Region, Product, Sales FROM T

-- The first two.
SELECT Region, Product, Sales FROM T LIMIT 2

-- Skip one, then take two.
SELECT Region, Product, Sales FROM T LIMIT 2 OFFSET 1

-- A limit larger than the result is not an error and truncates nothing.
SELECT Region, Product, Sales FROM T LIMIT 99

-- LIMIT 0 returns the header and no rows.
SELECT Region, Product, Sales FROM T LIMIT 0

-- LIMIT ALL is the same as no clause.
SELECT Region, Product, Sales FROM T LIMIT ALL
