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

/// Whether any engine supports `group`. Decided from vendor ids, usages and report lengths
/// only, so it needs no product string and no request to the device.
pub fn wants(group: &HidGroup) -> bool {
    HidppEngine::accepts(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::{Collection, CollectionId, GroupId};

    fn group(vid: u16, usage_page: u16, usage: u16, len: u16) -> HidGroup {
        HidGroup {
            id: GroupId(1),
            collections: vec![Collection {
                id: CollectionId(1),
                vid,
                pid: 0xC547,
                usage_page,
                usage,
                input_len: len,
                output_len: len,
                feature_len: 0,
                product: String::new(),
            }],
        }
    }

    #[test]
    fn wants_logitech_hidpp_without_product() {
        assert!(wants(&group(0x046D, 0xFF00, 0x0001, 7)));
        assert!(wants(&group(0x046D, 0xFF43, 0x0202, 20)));
    }

    #[test]
    fn does_not_want_other_groups() {
        assert!(!wants(&group(0x1234, 0xFF00, 0x0001, 7)));
        assert!(!wants(&group(0x046D, 0x0001, 0x0002, 7)));
    }
}
