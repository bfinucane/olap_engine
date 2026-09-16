-- First CREATE should succeed
CREATE TABLE Sales (Region STRING, Product STRING, M MEASURE)

-- Second CREATE with the same name must fail
CREATE TABLE Sales (Region STRING, Product STRING, M MEASURE)
