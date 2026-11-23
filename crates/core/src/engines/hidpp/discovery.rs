//! HID++ requests and the per-index discovery steps.
//!
//! Discovery of one device index is a chain of single requests: ping, Root
//! getFeature lookups, name, kind, unit id and the first battery read for a
//! HID++ 2.0 device; notification register setup and a battery register read
//! for a HID++ 1.0 one. [`advance`] decides the next step from a reply without
//! touching engine state.

use crate::model::DeviceKind;

use super::frame::{self, Reply};

// HID++ 1.0 registers and notification flags. Source: Solaar `hidpp10_constants.py`
// (`Registers`, `NotificationFlag`).
const GET_REGISTER: u8 = 0x81;
const SET_REGISTER: u8 = 0x80;
const REG_NOTIFICATIONS: u8 = 0x00;
pub(super) const REG_BATTERY_STATUS: u8 = 0x07;
pub(super) const REG_BATTERY_CHARGE: u8 = 0x0D;
/// `NotificationFlag.WIRELESS` 0x000100: byte 1, bit 0.
const WIRELESS_NOTIFICATIONS: u8 = 0x01;
/// `NotificationFlag.BATTERY_STATUS` 0x100000: byte 0, bit 4.
const BATTERY_NOTIFICATIONS: u8 = 0x10;

// HID++ 1.0 error codes. Source: Solaar `hidpp10_constants.py` `ErrorCode`.
const ERR_INVALID_SUB_ID: u8 = 0x01;
const ERR_INVALID_ADDRESS: u8 = 0x02;
/// The receiver could not reach the device (link down).
pub(super) const ERR_CONNECT_FAIL: u8 = 0x04;
/// No device paired on that index.
pub(super) const ERR_UNKNOWN_DEVICE: u8 = 0x08;
/// The device is busy or asleep.
pub(super) const ERR_RESOURCE_ERROR: u8 = 0x09;

/// Echoed by a ping reply. Source: Solaar `base.py` `ping()`.
const PING_MARK: u8 = 0x5A;

/// HID++ 2.0 features looked up through Root (index 0) getFeature, in this
/// order: device name and type, firmware info (unit id), then battery
/// features by preference (the first one present wins).
// Source: Solaar `hidpp20_constants.py` `SupportedFeature`; kernel hid-logitech-hidpp.c
// battery feature probing.
const FEATURES: [u16; 6] = [0x0005, 0x0003, 0x1004, 0x1000, 0x1001, 0x1F20];
const NAME_FEATURE: usize = 0;
const UNIT_FEATURE: usize = 1;
const FIRST_BATTERY_FEATURE: usize = 2;

/// How a device reports its battery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decoder {
    /// 0x1004 unified battery.
    Unified,
    /// 0x1000 battery level status.
    LevelStatus,
    /// 0x1001 battery voltage.
    Voltage,
    /// 0x1F20 ADC measurement (headsets).
    Adc,
    /// HID++ 1.0 register 0x0D (battery charge).
    Reg0D,
    /// HID++ 1.0 register 0x07 (battery status).
    Reg07,
}

impl Decoder {
    /// Decoder for battery entry `i` of [`FEATURES`].
    fn for_feature(i: usize) -> Self {
        match FEATURES[i] {
            0x1004 => Self::Unified,
            0x1000 => Self::LevelStatus,
            0x1001 => Self::Voltage,
            _ => Self::Adc,
        }
    }

    /// The request reading the battery; `fi` is the feature index (unused for registers).
    // Source: kernel hid-logitech-hidpp.c (0x1004 fn 1 get status; 0x1000, 0x1001, 0x1F20 fn 0).
    pub(super) fn read(self, fi: u8) -> Request {
        match self {
            Self::Unified => Request::feature(fi, 1, [0; 3]),
            Self::LevelStatus | Self::Voltage | Self::Adc => Request::feature(fi, 0, [0; 3]),
            Self::Reg0D => Request::get(REG_BATTERY_CHARGE),
            Self::Reg07 => Request::get(REG_BATTERY_STATUS),
        }
    }
}

/// One request we can match replies against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Request {
    Feature {
        fi: u8,
        function: u8,
        params: [u8; 3],
    },
    Register {
        sub_id: u8,
        address: u8,
        params: [u8; 3],
    },
}

impl Request {
    fn feature(fi: u8, function: u8, params: [u8; 3]) -> Self {
        Self::Feature {
            fi,
            function,
            params,
        }
    }

    fn get(address: u8) -> Self {
        Self::Register {
            sub_id: GET_REGISTER,
            address,
            params: [0; 3],
        }
    }

    fn set(address: u8, params: [u8; 3]) -> Self {
        Self::Register {
            sub_id: SET_REGISTER,
            address,
            params,
        }
    }

    pub(super) fn bytes(self, report_id: u8, index: u8) -> Vec<u8> {
        match self {
            Self::Feature {
                fi,
                function,
                params,
            } => frame::fap(report_id, index, fi, function, &params),
            Self::Register {
                sub_id,
                address,
                params,
            } => frame::request(report_id, index, sub_id, address, &params),
        }
    }

    pub(super) fn reply(self, bytes: &[u8], index: u8) -> Reply<'_> {
        match self {
            Self::Feature { fi, function, .. } => frame::match_fap(bytes, index, fi, function),
            Self::Register {
                sub_id, address, ..
            } => frame::match_register(bytes, index, sub_id, address),
        }
    }
}

/// Where a conversation stands: the request in flight is derived from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Receiver register 0x00 read, then written with the wireless flag added.
    GetNotifications,
    SetNotifications([u8; 3]),
    Ping,
    /// Root getFeature for [`FEATURES`]`[i]`.
    Feature(usize),
    NameLength,
    Name,
    Kind,
    UnitId,
    /// HID++ 1.0 device register 0x00 read, then written with the battery flag added.
    GetReg00,
    SetReg00([u8; 3]),
    /// First battery read of a device being discovered.
    Battery,
    /// Battery read of a known device; the engine builds and handles it.
    Read,
}

/// What discovery has learned about a device so far.
#[derive(Debug, Default)]
pub(super) struct Found {
    name_fi: u8,
    unit_fi: u8,
    pub(super) battery: Option<(Decoder, u8)>,
    name_len: usize,
    pub(super) name: Vec<u8>,
    pub(super) kind: Option<DeviceKind>,
    pub(super) unit: u32,
}

impl Found {
    fn after_features(&self) -> Step {
        if self.name_fi != 0 {
            Step::NameLength
        } else {
            self.after_kind()
        }
    }

    fn after_kind(&self) -> Step {
        if self.unit_fi != 0 {
            Step::UnitId
        } else {
            Step::Battery
        }
    }
}

/// What follows a reply.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next<'a> {
    /// Send the request for this step.
    Ask(Step),
    /// The first battery payload: discovery is complete.
    Discovered(&'a [u8]),
    /// The conversation is over.
    Done,
}

/// Device type from feature 0x0005 fn 2. Source: Solaar `hidpp20_constants.py` `DeviceKind`.
fn device_kind(t: u8) -> DeviceKind {
    match t {
        0 | 2 => DeviceKind::Keyboard,
        3..=5 => DeviceKind::Mouse,
        8 => DeviceKind::Headset,
        0x0B | 0x0C => DeviceKind::Controller,
        _ => DeviceKind::Other,
    }
}

/// The first three bytes of `p`, zero-padded.
fn first3(p: &[u8]) -> [u8; 3] {
    let mut out = [0; 3];
    let n = p.len().min(3);
    out[..n].copy_from_slice(&p[..n]);
    out
}

/// The request for discovery `step`; `None` for [`Step::Read`] or a step `f` cannot serve.
pub(super) fn request(step: Step, f: &Found) -> Option<Request> {
    Some(match step {
        Step::GetNotifications | Step::GetReg00 => Request::get(REG_NOTIFICATIONS),
        Step::SetNotifications(p) | Step::SetReg00(p) => Request::set(REG_NOTIFICATIONS, p),
        Step::Ping => Request::feature(0, 1, [0, 0, PING_MARK]),
        Step::Feature(i) => {
            let [hi, lo] = FEATURES[i].to_be_bytes();
            Request::feature(0, 0, [hi, lo, 0])
        }
        Step::NameLength => Request::feature(f.name_fi, 0, [0; 3]),
        Step::Name => Request::feature(f.name_fi, 1, [u8::try_from(f.name.len()).ok()?, 0, 0]),
        Step::Kind => Request::feature(f.name_fi, 2, [0; 3]),
        Step::UnitId => Request::feature(f.unit_fi, 0, [0; 3]),
        Step::Battery => {
            let (decoder, fi) = f.battery?;
            decoder.read(fi)
        }
        Step::Read => return None,
    })
}

/// The step after `reply` to discovery `step`, recording what it taught in `f`.
pub(super) fn advance<'a>(step: Step, f: &mut Found, reply: Reply<'a>) -> Next<'a> {
    // Unreachable, unpaired or asleep: stop at any step, register nothing.
    // Source: Solaar `base.py` (0x04 / 0x08 / 0x09 end a request to the device).
    if let Reply::Err10(ERR_CONNECT_FAIL | ERR_UNKNOWN_DEVICE | ERR_RESOURCE_ERROR) = reply {
        return Next::Done;
    }
    match step {
        Step::GetNotifications => match reply {
            // Only add the wireless flag; leave every other bit as it is.
            Reply::Ok(p) if p.get(1).is_some_and(|b| b & WIRELESS_NOTIFICATIONS == 0) => {
                let mut flags = first3(p);
                flags[1] |= WIRELESS_NOTIFICATIONS;
                Next::Ask(Step::SetNotifications(flags))
            }
            // Already on, or refused.
            _ => Next::Done,
        },
        Step::SetNotifications(_) | Step::Read => Next::Done,
        Step::Ping => match reply {
            Reply::Ok(&[major, ..]) if major >= 2 => Next::Ask(Step::Feature(NAME_FEATURE)),
            // Source: Solaar `base.py` `ping()`: invalid sub id means HID++ 1.0.
            Reply::Ok(_) | Reply::Err10(ERR_INVALID_SUB_ID) => {
                f.battery = Some((Decoder::Reg0D, 0));
                Next::Ask(Step::GetReg00)
            }
            // Unknown device, asleep or unreachable: nothing to register, no retry.
            _ => Next::Done,
        },
        Step::Feature(i) => {
            // Feature index 0 means absent.
            let fi = match reply {
                Reply::Ok(&[fi, ..]) => fi,
                _ => 0,
            };
            match i {
                NAME_FEATURE => {
                    f.name_fi = fi;
                    Next::Ask(Step::Feature(UNIT_FEATURE))
                }
                UNIT_FEATURE => {
                    f.unit_fi = fi;
                    Next::Ask(Step::Feature(FIRST_BATTERY_FEATURE))
                }
                _ if fi != 0 => {
                    f.battery = Some((Decoder::for_feature(i), fi));
                    Next::Ask(f.after_features())
                }
                _ if i + 1 < FEATURES.len() => Next::Ask(Step::Feature(i + 1)),
                // No battery feature: nothing to show.
                _ => Next::Done,
            }
        }
        Step::NameLength => match reply {
            Reply::Ok(&[len, ..]) if len > 0 => {
                f.name_len = len.into();
                Next::Ask(Step::Name)
            }
            _ => Next::Ask(Step::Kind),
        },
        Step::Name => {
            if let Reply::Ok(p) = reply {
                let take = (f.name_len - f.name.len()).min(p.len());
                f.name.extend_from_slice(&p[..take]);
                if take > 0 && f.name.len() < f.name_len {
                    return Next::Ask(Step::Name);
                }
            }
            Next::Ask(Step::Kind)
        }
        Step::Kind => {
            if let Reply::Ok(&[t, ..]) = reply {
                f.kind = Some(device_kind(t));
            }
            Next::Ask(f.after_kind())
        }
        Step::UnitId => {
            // Source: Solaar `hidpp20.py` `get_ids` (0x0003 fn 0: unit id in bytes 1..5).
            if let Reply::Ok(&[_, u0, u1, u2, u3, ..]) = reply {
                f.unit = u32::from_be_bytes([u0, u1, u2, u3]);
            }
            Next::Ask(Step::Battery)
        }
        Step::GetReg00 => match reply {
            // Only add the battery flag; leave every other bit as it is.
            Reply::Ok(p) if p.first().is_some_and(|b| b & BATTERY_NOTIFICATIONS == 0) => {
                let mut flags = first3(p);
                flags[0] |= BATTERY_NOTIFICATIONS;
                Next::Ask(Step::SetReg00(flags))
            }
            _ => Next::Ask(Step::Battery),
        },
        Step::SetReg00(_) => Next::Ask(Step::Battery),
        Step::Battery => match reply {
            // Register 0x0D unsupported: fall back to 0x07.
            Reply::Err10(ERR_INVALID_ADDRESS) if f.battery == Some((Decoder::Reg0D, 0)) => {
                f.battery = Some((Decoder::Reg07, 0));
                Next::Ask(Step::Battery)
            }
            Reply::Ok(p) => Next::Discovered(p),
            _ => Next::Done,
        },
    }
}
