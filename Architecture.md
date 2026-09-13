# Architecture Notes
* Flat vs. Pivot: Standard SQL returns flat tables. However, if a cube is defined with a MEASURE dimension, the SQL engine will perform a "Measure Pivot" to return standard relational 2D results.
* Persistence: The database automatically saves its state to database.bin upon typing .exit and reloads it on startup.


