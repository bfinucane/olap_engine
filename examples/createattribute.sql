-- 1. Create the Attribute Table
CREATE ATTRIBUTE TABLE Employees (Name STRING, Property STRING)

-- 2. Insert mixed data types!
INSERT INTO Employees VALUES ('Alice', 'Department', 'Sales')
INSERT INTO Employees VALUES ('Alice', 'Salary', 95000)
INSERT INTO Employees VALUES ('Bob', 'Department', 'IT')

-- 3. Query it!
SELECT Property, value FROM Employees WHERE Name = 'Alice'