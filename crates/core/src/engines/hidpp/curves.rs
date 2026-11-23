//! Millivolt-to-percent curves for devices that only report battery voltage.

/// A named discharge curve. Points run from full to empty by descending millivolts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoltageCurve {
    /// Generic Li-ion curve used for feature 0x1001.
    SolaarLiIon,
    /// Default for feature 0x1F20 headsets.
    HeadsetControlG633,
    /// Logitech PRO X Wireless headset.
    ProXWirelessGhub,
}

// Source: Solaar `hidpp20.py` `estimate_battery_level_percentage` (GPLv2+).
const SOLAAR_LIION: &[(u16, u8)] = &[
    (4186, 100),
    (4067, 90),
    (3989, 80),
    (3922, 70),
    (3859, 60),
    (3811, 50),
    (3778, 40),
    (3751, 30),
    (3717, 20),
    (3671, 10),
    (3646, 5),
    (3579, 2),
    (3500, 0),
];

// Source: HeadsetControl `logitech_calibrations.hpp` `LOGITECH_G633` (GPLv3).
const HEADSETCONTROL_G633: &[(u16, u8)] = &[
    (4100, 100),
    (3950, 80),
    (3850, 60),
    (3750, 40),
    (3650, 20),
    (3500, 10),
    (3300, 5),
    (3150, 0),
];

// Source: G HUB-derived PRO X Wireless curve, verified against G HUB 2026.7 (design §10).
const PRO_X_WIRELESS_GHUB: &[(u16, u8)] = &[
    (4102, 100),
    (4046, 95),
    (3993, 90),
    (3950, 85),
    (3915, 80),
    (3877, 75),
    (3838, 70),
    (3809, 65),
    (3789, 60),
    (3772, 55),
    (3758, 50),
    (3747, 45),
    (3736, 40),
    (3722, 35),
    (3707, 30),
    (3687, 25),
    (3661, 20),
    (3644, 15),
    (3548, 10),
    (3426, 5),
    (3293, 0),
];

impl VoltageCurve {
    fn points(self) -> &'static [(u16, u8)] {
        match self {
            Self::SolaarLiIon => SOLAAR_LIION,
            Self::HeadsetControlG633 => HEADSETCONTROL_G633,
            Self::ProXWirelessGhub => PRO_X_WIRELESS_GHUB,
        }
    }
}

/// Battery percent for `mv` on `curve`: linear between the bracketing points, truncated.
pub fn percent(curve: VoltageCurve, mv: u16) -> u8 {
    let points = curve.points();
    let (top_mv, top_pct) = points[0];
    if mv >= top_mv {
        return top_pct;
    }
    for pair in points.windows(2) {
        let ((v_hi, p_hi), (v_lo, p_lo)) = (pair[0], pair[1]);
        if mv >= v_lo {
            let pct = u32::from(p_lo)
                + u32::from(p_hi - p_lo) * u32::from(mv - v_lo) / u32::from(v_hi - v_lo);
            return u8::try_from(pct.min(100)).unwrap_or(100);
        }
    }
    0
}

/// The curve a catalog quirk names, e.g. `"pro-x-wireless-ghub"`.
pub fn curve_by_name(name: &str) -> Option<VoltageCurve> {
    match name {
        "pro-x-wireless-ghub" => Some(VoltageCurve::ProXWirelessGhub),
        "headsetcontrol-g633" => Some(VoltageCurve::HeadsetControlG633),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(curve: VoltageCurve, cases: &[(u16, u8)]) {
        for &(mv, want) in cases {
            assert_eq!(percent(curve, mv), want, "{curve:?} at {mv} mV");
        }
    }

    #[test]
    fn solaar_liion_points() {
        check(
            VoltageCurve::SolaarLiIon,
            &[
                (5000, 100),
                (4186, 100),
                (4028, 85),
                (3811, 50),
                (3700, 16),
                (3500, 0),
                (3499, 0),
            ],
        );
    }

    #[test]
    fn headsetcontrol_g633_points() {
        check(
            VoltageCurve::HeadsetControlG633,
            &[
                (4200, 100),
                (4100, 100),
                (4000, 86),
                (3850, 60),
                (3150, 0),
                (3100, 0),
            ],
        );
    }

    #[test]
    fn prox_ghub_points() {
        check(
            VoltageCurve::ProXWirelessGhub,
            &[
                (4102, 100),
                (3993, 90),
                (3838, 70),
                (3758, 50),
                (3661, 20),
                (3293, 0),
                (3292, 0),
                (3971, 87),
                (3828, 68),
                (3819, 66),
            ],
        );
    }

    #[test]
    fn names_resolve() {
        assert_eq!(
            curve_by_name("pro-x-wireless-ghub"),
            Some(VoltageCurve::ProXWirelessGhub)
        );
        assert_eq!(
            curve_by_name("headsetcontrol-g633"),
            Some(VoltageCurve::HeadsetControlG633)
        );
        assert_eq!(curve_by_name("nonexistent"), None);
    }
}
