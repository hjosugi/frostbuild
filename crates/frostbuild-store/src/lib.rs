//! Stable storage facade for the graph store, journal, hash cache and CAS.
//! Keeping this crate boundary lets the formats evolve without coupling the
//! CLI or daemon to their concrete on-disk representation.

pub use frostbuild_core::cas::LocalCas;
pub use frostbuild_core::graph_store::GraphStore;
pub use frostbuild_core::hashcache::HashCache;
pub use frostbuild_core::journal::{Journal, JournalEntry};
