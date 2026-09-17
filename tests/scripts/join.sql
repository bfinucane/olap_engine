-- ATTRIBUTE JOIN: enrich a transactional cube with columns from an
-- attribute table, keyed by a shared (conformed) dimension.
--
-- Because the ONLY thing two cubes can join on is a conformed dimension
-- (a dimension with the same name in both cubes), the standard `USING (col)`
-- clause names the join key exactly once. `ON (...)` is rejected with guidance.

-- 1. Primary (transactional) cube and an attribute table sharing 'Account'.
CREATE TABLE Transactions (Region STRING, Account STRING, Acc MEASURE)
CREATE ATTRIBUTE TABLE Account_Attr (Account STRING, Property MEASURE)

INSERT INTO Transactions VALUES ('NA', '1000', 'Sales', 500)
INSERT INTO Transactions VALUES ('NA', '1000', 'Units', 50)
INSERT INTO Transactions VALUES ('NA', '2000', 'Sales', 300)
INSERT INTO Transactions VALUES ('NA', '2000', 'Units', 75)

INSERT INTO Account_Attr VALUES ('1000', 'Name', 'Cash Account')
INSERT INTO Account_Attr VALUES ('1000', 'Type', 'Asset')
INSERT INTO Account_Attr VALUES ('2000', 'Name', 'Revenue Account')
INSERT INTO Account_Attr VALUES ('2000', 'Type', 'Income')

-- 2. Explicit USING form: join attributes onto the conformed 'Account' axis.
--    Rows follow the Account dimension's display order (creation order).
SELECT Account, Name, Type, Sales, Units FROM Transactions JOIN Account_Attr USING (Account)

-- 3. Constraint omitted: inferred because exactly one dimension conforms.
SELECT Account, Name, Sales FROM Transactions JOIN Account_Attr

-- 4. An attribute missing for a key renders as '-' (left-join null handling).
INSERT INTO Transactions VALUES ('NA', '3000', 'Sales', 120)
SELECT Account, Name, Type, Sales FROM Transactions JOIN Account_Attr USING (Account)

-- 5. USING names a dimension that is not conformed to both cubes -> error.
SELECT Account, Name, Sales FROM Transactions JOIN Account_Attr USING (Region)

-- 6. The join key must be projected to key the lookup -> error.
SELECT Name, Sales FROM Transactions JOIN Account_Attr USING (Account)

-- 7. Unknown column -> error.
SELECT Account, Bogus, Sales FROM Transactions JOIN Account_Attr USING (Account)

-- 8. ON is not supported -> guidance to use USING.
SELECT Account, Name, Sales FROM Transactions JOIN Account_Attr ON Account = Account

-- 9. No conformed dimension at all -> error. 'Other' keys on Product,
--    'Solo_Attr' keys on a different dimension name, so nothing conforms.
CREATE TABLE Other (Product STRING, Amt MEASURE)
CREATE ATTRIBUTE TABLE Solo_Attr (Solo STRING, Property MEASURE)
SELECT Product, value FROM Other JOIN Solo_Attr

-- 10. Two *other* cubes that share TWO dimensions without USING -> ambiguous.
CREATE TABLE A (Dim1 STRING, Dim2 STRING, V MEASURE)
CREATE ATTRIBUTE TABLE B (Dim1 STRING, Dim2 STRING, Property MEASURE)
SELECT Dim1, V FROM A JOIN B
