//! KARON3 Metrics Module
pub mod counters;
// prometheus submodule disabled: depends on crate::config::runtime which is
// not reachable from the current module tree (config.rs vs config/ conflict).
// pub mod prometheus;
