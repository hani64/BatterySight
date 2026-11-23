//! Built-in protocol engines.

pub mod hidpp;

use crate::catalog::Catalog;
use crate::engine::Engine;
use crate::hid::HidGroup;

/// The engine that speaks to `group`, or `None` if no engine supports it.
pub fn engine_for(_group: &HidGroup, _catalog: &Catalog) -> Option<Box<dyn Engine>> {
    None
}
