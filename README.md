# Patricia OLAP Engine

An experimental, high-performance, in-memory Multidimensional OLAP database written in Rust. 

Inspired by enterprise OLAP systems like IBM TM1 and Infor OLAP Server, Patria uses a **Sparse Patricia Trie** for storage and supports **Parent-Child Ragged Hierarchies** with Just-In-Time (JIT) aggregations. It wraps this multidimensional core in a standard ANSI SQL parser for easy querying.

## Features

* **Sparse Trie Storage:** Memory-efficient storage that only consumes RAM for populated coordinates.
* **Tokenized Dictionaries:** Zero string-duplication. Strings are converted to integers at the engine's edge.
* **Ragged Hierarchies:** Support for unbalanced, parent-child dimension graphs with fractional consolidation weights (e.g., `Profit = Revenue (1.0) + Expenses (-1.0)`).
* **JIT Aggregation:** Consolidations are calculated dynamically on read, preventing database explosion.
* **Multi-Type Cubes:** Supports standard numeric/transactional cubes and string-based Attribute cubes.
* **SQL Interface:** Query multidimensional slices using standard `SELECT` statements with dynamic Measure Pivoting.
* **Zero-Dependency Core:** The engine relies on Rust's standard library, `sqlparser`, and `bincode` for binary persistence.

See [ROADMAP.md](ROADMAP.md) for the planned direction (client/server, security,
calculations, and multidimensional syntax).

## Getting Started

### Prerequisites
* [Rust](https://www.rust-lang.org/tools/install) (1.70+)

### Build and Run
Clone the repository and start the interactive REPL shell:
```bash
git clone https://github.com/YOUR_USERNAME/olap_engine.git
cd olap_engine
cargo run