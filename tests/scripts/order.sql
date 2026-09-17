-- Design-time member ordering (tree-native, "Model 1").
-- The hierarchy itself defines the order: roots in creation order, and each
-- parent's children in sibling order. Run-time queries follow this order.
-- Reordering happens among SIBLINGS; moving across parents is rejected.
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

-- Give Region a hierarchy. All four leaves become children of Zones, so the
-- flat order becomes a depth-first tree order (Zones, then its children).
CREATE TABLE Sales2 (Region STRING, Product STRING)
.rollup Region Zones West 1.0
.rollup Region Zones East 1.0
.rollup Region Zones North 1.0
.rollup Region Zones South 1.0
.order Region
.tree Region Zones

-- One-parent-per-hierarchy: re-parenting North under 'Coast' auto-detaches
-- it from Zones. West (under Zones) and North (under Coast) are now in
-- different hierarchies, so they cannot be ordered relative to each other
-- and the cross-parent move is REJECTED.
.rollup Region Coast North 1.0
.order Region West before North
.tree Region Zones
.tree Region Coast

