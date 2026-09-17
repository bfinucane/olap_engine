// `cube` module nested inside the `cube` directory (see catalog/mod.rs for
// the rationale behind this deliberate `module_inception`).
#[allow(clippy::module_inception)]
pub mod cube;
pub mod node;
// query.rs is no longer needed/used in this design