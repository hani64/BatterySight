//! Built-in protocol engines.

pub mod hidpp;

use crate::catalog::Catalog;
use crate::engine::Engine;
use crate::hid::HidGroup;

use hidpp::HidppEngine;

/// The engine that speaks to `group`, or `None` if no engine supports it.
pub fn engine_for(group: &HidGroup, catalog: &Catalog) -> Option<Box<dyn Engine>> {
    if HidppEngine::accepts(group) {
        return Some(Box::new(HidppEngine::new(group, catalog)));
    }
    None
}
