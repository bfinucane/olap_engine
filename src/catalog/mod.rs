// `catalog` module nested inside the `catalog` directory. The nested path
// (`catalog::catalog::Catalog`) is deliberate: it keeps each concern in its
// own folder and leaves room for sibling submodules later.
#[allow(clippy::module_inception)]
pub mod catalog;