-- Design-time member ordering.
-- Rows default to member CREATION order. A cube author can reorder a
-- dimension at design time, and run-time queries then follow that order.
CREATE TABLE Sales (Region STRING, Product STRING)

-- Creation order: West, East, North, South.
INSERT INTO Sales VALUES ('West', 'Widget', 10)
INSERT INTO Sales VALUES ('East', 'Widget', 20)
INSERT INTO Sales VALUES ('North', 'Widget', 30)
INSERT INTO Sales VALUES ('South', 'Widget', 40)

-- Default: creation order.
SELECT Region, value FROM Sales

-- Show / author the display order: put South first, then East before North.
.order Region
.order Region South front
.order Region East before North
.order Region

-- Queries now honour the authored order.
SELECT Region, value FROM Sales
