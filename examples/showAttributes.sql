-- 1. Create cubes (Make sure you use MEASURE for the property columns)
CREATE TABLE Transactions (Region STRING, Account STRING, Acc MEASURE)
CREATE ATTRIBUTE TABLE Account_Attr (Account STRING, Property MEASURE)

-- 2. Insert Transactions
INSERT INTO Transactions VALUES ('NA', '1000', 'Sales', 500)
INSERT INTO Transactions VALUES ('NA', '1000', 'Units', 50)
INSERT INTO Transactions VALUES ('NA', '2000', 'Sales', 300)

-- 3. Insert Attributes
INSERT INTO Account_Attr VALUES ('1000', 'Name', 'Cash Account')
INSERT INTO Account_Attr VALUES ('1000', 'Type', 'Asset')
INSERT INTO Account_Attr VALUES ('2000', 'Name', 'Revenue Account')
INSERT INTO Account_Attr VALUES ('2000', 'Type', 'Income')

-- 4. Execute the JOIN Query
SELECT Account, Name, Type, Sales, Units FROM Transactions JOIN Account_Attr