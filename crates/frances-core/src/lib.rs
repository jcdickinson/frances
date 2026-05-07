//! Shared utilities used across the Frances workspace. Currently just the
//! `JsonRepair` newtype, which absorbs a model bug where array tool-call
//! arguments arrive double-encoded as JSON strings.

pub mod json_repair;

pub use json_repair::JsonRepair;
