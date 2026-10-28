-- ORDER BY must name an output column or a 1-based position. Anything else is a
-- LOUD error, never a silently ignored clause: before the fix the whole ORDER BY
-- was dropped and these rows came back in display order with no error at all.
--
-- This fixture is expected to exit non-zero (see EXPECT_FAILURE in
-- tests/script_tests.rs, which is explicit data, not inferred from this text).

CREATE TABLE T (Region STRING, M MEASURE)

INSERT INTO T VALUES ('East', 'Sales', 10)
INSERT INTO T VALUES ('West', 'Sales', 20)

-- The output columns are Region and Sales, so "Nope" cannot be resolved. The
-- message must name the unknown column AND list what this query does return.
SELECT Region, Sales FROM T ORDER BY Nope
