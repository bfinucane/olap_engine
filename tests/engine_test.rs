use olap_engine::catalog::catalog::Catalog;
use olap_engine::engine::sql::execute_sql;

#[test]
fn test_basic_consolidation() {
    let mut catalog = Catalog::new();

    // Setup Hierarchy
    let dim_geo = catalog.get_or_create_dimension("Geography");
    {
        let mut geo = dim_geo.write().unwrap();
        geo.add_component("Europe", "France", 1.0);
        geo.add_component("Europe", "Germany", 1.0);
    }

    // Setup Cube
    catalog.add_cube("Sales", &["Geography", "Product"]);
    let sales_cube = catalog.get_cube_mut("Sales").unwrap();

    // Insert Data
    sales_cube.write(&["France", "Laptop"], 100.0);
    sales_cube.write(&["Germany", "Laptop"], 150.0);

    // Query Data
    let result = sales_cube.query_consolidated(&["Europe", "Laptop"]);
    
    // Assert the result is exactly what we expect!
    assert_eq!(result, 250.0);
}


#[test]
fn test_sql_query_parser() {
    let mut catalog = Catalog::new();

    // 1. Setup Data
    catalog.get_or_create_dimension("Geography");
    catalog.get_or_create_dimension("Product");
    catalog.add_cube("Sales", &["Geography", "Product"]);
    
    let sales_cube = catalog.get_cube_mut("Sales").unwrap();
    sales_cube.write(&["France", "Laptop"], 100.0);

    // 2. Define the SQL string
    let sql = "SELECT value FROM Sales WHERE Geography = 'France' AND Product = 'Laptop'";

    // 3. Execute!
    let result = execute_sql(&mut catalog, sql).unwrap();

    // 4. Assert
    assert_eq!(result, "100");
}