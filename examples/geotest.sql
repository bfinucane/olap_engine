CREATE TABLE Financials (Scenario STRING, Geo STRING, Acc MEASURE)
INSERT INTO Financials VALUES ('Actual', 'France', 'Sales', 100)
INSERT INTO Financials VALUES ('Budget', 'France', 'Sales', 500)
-- This will ONLY return 100, because 'Actual' was created first and became the default!
SELECT Sales FROM Financials WHERE Geo = 'France'