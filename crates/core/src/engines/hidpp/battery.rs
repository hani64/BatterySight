//! Decoders turning HID++ battery replies and notifications into readings.

use crate::model::{Charge, CoarseLevel, Level};

use super::curves::{self, VoltageCurve};

/// One decoded battery report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub level: Option<Level>,
    pub millivolts: Option<u16>,
    pub charge: Charge,
    pub online: bool,
}

/// Returned when the payload is too short to decode.
const NO_READING: Reading = Reading {
    level: None,
    millivolts: None,
    charge: Charge::Unknown,
    online: false,
};

fn online(level: Option<Level>, millivolts: Option<u16>, charge: Charge) -> Reading {
    Reading {
        level,
        millivolts,
        charge,
        online: true,
    }
}

/// Coarse level from a bit field where bit3 Full, bit2 Good, bit1 Low, bit0 Critical;
/// the highest set bit wins.
fn coarse_from_bits(bits: u8) -> Option<Level> {
    let coarse = if bits & 0x08 != 0 {
        CoarseLevel::Full
    } else if bits & 0x04 != 0 {
        CoarseLevel::Good
    } else if bits & 0x02 != 0 {
        CoarseLevel::Low
    } else if bits & 0x01 != 0 {
        CoarseLevel::Critical
    } else {
        return None;
    };
    Some(Level::Coarse(coarse))
}

/// Feature 0x1000 (battery level status): `p[0]` percent, `p[2]` status.
// Source: Linux kernel hid-logitech-hidpp `hidpp20_batterylevel_map_status_capacity` (facts only).
pub fn decode_1000(p: &[u8]) -> Reading {
    let &[pct, _, status, ..] = p else {
        return NO_READING;
    };
    let charge = match status {
        0 => Charge::Discharging,
        1 | 2 | 4 => Charge::Charging,
        3 => Charge::Full,
        _ => Charge::NotCharging,
    };
    let level = match status {
        3 => Some(Level::Percent(100)),
        0 => Some(Level::Percent(pct)),
        _ if pct == 0 => None,
        _ => Some(Level::Percent(pct)),
    };
    online(level, None, charge)
}

/// Feature 0x1001 (battery voltage): big-endian mV in `p[0..2]`, flags in `p[2]`.
// Source: Linux kernel hid-logitech-hidpp `hidpp20_battery_map_status_voltage` (facts only).
pub fn decode_1001(p: &[u8]) -> Reading {
    let &[hi, lo, flags, ..] = p else {
        return NO_READING;
    };
    let mv = u16::from_be_bytes([hi, lo]);
    let charge = if flags & 0x80 != 0 {
        match flags & 0x07 {
            0 => Charge::Charging,
            1 => Charge::Full,
            2 => Charge::NotCharging,
            _ => Charge::Unknown,
        }
    } else {
        Charge::Discharging
    };
    let level = Level::Percent(curves::percent(VoltageCurve::SolaarLiIon, mv));
    online(Some(level), Some(mv), charge)
}

/// Feature 0x1004 (unified battery): `p[0]` state of charge, `p[1]` coarse level bits,
/// `p[2]` charging status.
// Source: Linux kernel hid-logitech-hidpp `hidpp20_unifiedbattery_map_status` and
// `hidpp20_unifiedbattery_map_level` (facts only).
pub fn decode_1004(p: &[u8]) -> Reading {
    let &[soc, bits, status, ..] = p else {
        return NO_READING;
    };
    let level = if (1..=100).contains(&soc) {
        Some(Level::Percent(soc))
    } else {
        coarse_from_bits(bits)
    };
    let charge = match status {
        0 => Charge::Discharging,
        1 | 2 => Charge::Charging,
        3 => Charge::Full,
        _ => Charge::NotCharging,
    };
    online(level, None, charge)
}

/// Feature 0x1F20 (ADC measurement): big-endian mV in `p[0..2]`, flags in `p[2]`.
/// Flags other than 0x01/0x03/0x07 mean the headset is off or out of range.
// Source: Linux kernel hid-logitech-hidpp `hidpp20_map_adc_measurement_1f20` (facts only).
pub fn decode_1f20(p: &[u8], curve: VoltageCurve) -> Reading {
    let &[hi, lo, flags, ..] = p else {
        return NO_READING;
    };
    let charge = match flags {
        0x01 => Charge::Discharging,
        0x03 => Charge::Charging,
        0x07 => Charge::Full,
        _ => return NO_READING,
    };
    let mv = u16::from_be_bytes([hi, lo]);
    online(
        Some(Level::Percent(curves::percent(curve, mv))),
        Some(mv),
        charge,
    )
}

/// HID++ 1.0 register 0x0D (battery mileage): `p[0]` percent, status in `p[2] >> 6`.
/// Takes `frame[4..]` for a register reply and `frame[3..]` for a notification.
// Source: Linux kernel hid-logitech-hidpp `hidpp10_battery_mileage_map_status` (facts only);
// Solaar `hidpp10.parse_battery_status` (status `p[2] & 0xF0`: 0x30/0x50/0x90 agree).
// [UNVERIFIED] kernel/Solaar disagree on 0x0D notification offset; following Solaar
pub fn decode_reg_0d(p: &[u8]) -> Reading {
    let &[pct, _, status, ..] = p else {
        return NO_READING;
    };
    let charge = match status >> 6 {
        0 => Charge::Discharging,
        1 => Charge::Charging,
        2 => Charge::Full,
        _ => Charge::NotCharging,
    };
    online(Some(Level::Percent(pct)), None, charge)
}

/// HID++ 1.0 register 0x07 (battery status): `p[0]` level 1..=7, `p[1]` status.
/// Takes `frame[4..]` for a register reply and `frame[3..]` for a notification.
// Source: Linux kernel hid-logitech-hidpp `hidpp10_battery_status_map_level` and
// `hidpp10_battery_status_map_status` (facts only).
pub fn decode_reg_07(p: &[u8]) -> Reading {
    let &[raw_level, status, ..] = p else {
        return NO_READING;
    };
    let level = match raw_level {
        1 | 2 => Some(CoarseLevel::Critical),
        3 | 4 => Some(CoarseLevel::Low),
        5 | 6 => Some(CoarseLevel::Good),
        7 => Some(CoarseLevel::Full),
        _ => None,
    };
    let charge = match status {
        0x00 => Charge::Discharging,
        0x21 | 0x24 | 0x25 => Charge::Charging,
        0x22 | 0x26 => Charge::Full,
        0x20 => Charge::Unknown,
        _ => Charge::NotCharging,
    };
    online(level.map(Level::Coarse), None, charge)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(level: Option<Level>, millivolts: Option<u16>, charge: Charge) -> Reading {
        Reading {
            level,
            millivolts,
            charge,
            online: true,
        }
    }

    #[test]
    fn b1000_full_reports_100() {
        assert_eq!(
            decode_1000(&[0, 0, 3]),
            reading(Some(Level::Percent(100)), None, Charge::Full)
        );
    }

    #[test]
    fn b1000_charging_unknown_percent() {
        assert_eq!(
            decode_1000(&[0, 0, 1]),
            reading(None, None, Charge::Charging)
        );
    }

    #[test]
    fn b1000_discharging_and_other_status() {
        assert_eq!(
            decode_1000(&[42, 0, 0]),
            reading(Some(Level::Percent(42)), None, Charge::Discharging)
        );
        assert_eq!(
            decode_1000(&[42, 0, 4]),
            reading(Some(Level::Percent(42)), None, Charge::Charging)
        );
        assert_eq!(
            decode_1000(&[42, 0, 6]),
            reading(Some(Level::Percent(42)), None, Charge::NotCharging)
        );
    }

    #[test]
    fn b1004_soc() {
        assert_eq!(
            decode_1004(&[64, 4, 0, 0]),
            reading(Some(Level::Percent(64)), None, Charge::Discharging)
        );
    }

    #[test]
    fn b1004_coarse_when_no_soc() {
        assert_eq!(
            decode_1004(&[0, 2, 0, 0]),
            reading(
                Some(Level::Coarse(CoarseLevel::Low)),
                None,
                Charge::Discharging
            )
        );
    }

    #[test]
    fn b1004_coarse_highest_bit_wins_and_none_without_bits() {
        assert_eq!(
            decode_1004(&[0, 0x0F, 0, 0]).level,
            Some(Level::Coarse(CoarseLevel::Full))
        );
        assert_eq!(
            decode_1004(&[101, 0x01, 0, 0]).level,
            Some(Level::Coarse(CoarseLevel::Critical))
        );
        assert_eq!(decode_1004(&[0, 0, 0, 0]).level, None);
    }

    #[test]
    fn b1004_charging() {
        assert_eq!(decode_1004(&[80, 4, 2, 1]).charge, Charge::Charging);
    }

    #[test]
    fn b1001_charging_flag() {
        assert_eq!(
            decode_1001(&[0x0F, 0x8C, 0x80]),
            reading(Some(Level::Percent(78)), Some(3980), Charge::Charging)
        );
    }

    #[test]
    fn b1001_discharging_without_external_power() {
        let r = decode_1001(&[0x0F, 0x8C, 0x01]);
        assert_eq!(r.charge, Charge::Discharging);
        assert_eq!(r.millivolts, Some(3980));
    }

    #[test]
    fn adc_discharging_prox() {
        assert_eq!(
            decode_1f20(&[0x0F, 0x83, 0x01], VoltageCurve::ProXWirelessGhub),
            reading(Some(Level::Percent(87)), Some(3971), Charge::Discharging)
        );
    }

    #[test]
    fn adc_unknown_flags_means_offline() {
        let r = decode_1f20(&[0x0F, 0x83, 0x0F], VoltageCurve::ProXWirelessGhub);
        assert!(!r.online);
        assert_eq!(r.level, None);
        assert_eq!(r.charge, Charge::Unknown);
    }

    #[test]
    fn reg0d_charging() {
        assert_eq!(
            decode_reg_0d(&[55, 0, 0x40]),
            reading(Some(Level::Percent(55)), None, Charge::Charging)
        );
    }

    #[test]
    fn reg07_levels() {
        assert_eq!(
            decode_reg_07(&[3, 0x00]),
            reading(
                Some(Level::Coarse(CoarseLevel::Low)),
                None,
                Charge::Discharging
            )
        );
        assert_eq!(
            decode_reg_07(&[7, 0x22]),
            reading(Some(Level::Coarse(CoarseLevel::Full)), None, Charge::Full)
        );
    }

    #[test]
    fn reg07_out_of_range_level_is_none() {
        assert_eq!(decode_reg_07(&[0, 0x00]).level, None);
        assert_eq!(decode_reg_07(&[8, 0x00]).level, None);
    }

    #[test]
    fn short_input_yields_no_reading() {
        assert_eq!(decode_1000(&[50, 0]), NO_READING);
        assert_eq!(decode_1001(&[0x0F, 0x8C]), NO_READING);
        assert_eq!(decode_1004(&[64, 4]), NO_READING);
        assert_eq!(
            decode_1f20(&[0x0F, 0x83], VoltageCurve::HeadsetControlG633),
            NO_READING
        );
        assert_eq!(decode_reg_0d(&[55]), NO_READING);
        assert_eq!(decode_reg_07(&[3]), NO_READING);
        assert_eq!(decode_reg_07(&[]), NO_READING);
    }
}
