//! HID collection types.

/// Identifies one opened HID top-level collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectionId(pub u32);

/// Identifies the set of collections belonging to one physical device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(pub u32);

/// A protocol-defined role assigned to a specific collection by its engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectionRole(pub u8);

/// Operation requested on an opened HID collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    Output,
    SetFeature,
    GetFeature,
}

/// How a received report reached the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportOrigin {
    Input,
    FeatureReply,
}

/// One HID top-level collection; report lengths are in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: CollectionId,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    /// USB interface number from the PnP instance ID, when present.
    pub interface: Option<u8>,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
    /// Product metadata for pure matching, or the display name after runtime admission.
    pub product: String,
}

/// All collections exposed by one physical device or receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidGroup {
    pub id: GroupId,
    pub collections: Vec<Collection>,
}
