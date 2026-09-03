-- This is my demo setup script
CREATE TABLE Financials (Region STRING, Account STRING, Month STRING)

-- Load some data
INSERT INTO Financials VALUES ('North America', 'Revenue', 'Jan', 50000)
INSERT INTO Financials VALUES ('North America', 'Expenses', 'Jan', 30000)

-- Build the Account hierarchy (Revenue - Expenses = Profit)
.rollup Account Profit Revenue 1.0
.rollup Account Profit Expenses -1.0

-- Query the JIT aggregation!
SELECT Account, value FROM Financials WHERE Region = 'North America' AND Month = 'Jan'