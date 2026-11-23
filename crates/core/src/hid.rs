//! HID collection types.

/// Identifies one opened HID top-level collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionId(pub u32);

/// Identifies the set of collections belonging to one physical device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(pub u32);

/// One HID top-level collection; report lengths are in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: CollectionId,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
    pub product: String,
}

/// All collections exposed by one physical device or receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidGroup {
    pub id: GroupId,
    pub collections: Vec<Collection>,
}
