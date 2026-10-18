CREATE TABLE Sales (Region STRING, Segment STRING, Category STRING)
.import examples/Plateau/cube-region-segment-category.csv Sales
CREATE HIERARCHY Product_Class IN Category
.import_lvl examples/Plateau/hierarchy-product.csv Product_Class
SELECT Region, value FROM Sales
