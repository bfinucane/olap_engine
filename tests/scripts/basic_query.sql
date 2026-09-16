-- Happy path: create a cube, insert rows, and read them back.
CREATE TABLE Financials (Scenario STRING, Geo STRING, Acc MEASURE)

INSERT INTO Financials VALUES ('Actual', 'France', 'Sales', 100)
INSERT INTO Financials VALUES ('Budget', 'France', 'Sales', 500)

-- Single-cell read via the default filter path.
SELECT Sales FROM Financials WHERE Scenario = 'Actual' AND Geo = 'France'
