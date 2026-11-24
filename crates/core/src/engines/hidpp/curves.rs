//! Millivolt-to-percent curves for devices that only report battery voltage.

use crate::model::Charge;

/// A named voltage curve: a discharge table, and for some devices a charging table.
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

// Source: G HUB `pro_headset/pro_x_wireless_headset_battery.xml` discharge curve, matched
// against G HUB 2026.7.
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

/// A G HUB charging model: SoC while the voltage rises under constant current, then while it
/// falls from its peak under constant voltage (topping). SoC is in hundredths of a percent.
struct ChargingCurve {
    /// (mV, SoC) by ascending millivolts; a voltage may repeat with rising SoC.
    ascending: &'static [(u16, u16)],
    /// (mV, SoC) by rising SoC; the voltage falls with noise.
    descending: &'static [(u16, u16)],
    /// A charge whose peak reached this voltage and then fell is topping.
    peak_region_mv: u16,
}

// Source: G HUB `pro_headset/pro_x_wireless_headset_battery.xml`
// `<charging format="charging_topping">`, every point; volts and SoC scaled by 1000 and 100
// and rounded. The peak region starts 20 mV below the ascending table's top (4.275 V).
const PRO_X_WIRELESS_GHUB_CHARGING: ChargingCurve = ChargingCurve {
    ascending: PRO_X_WIRELESS_GHUB_ASCENDING,
    descending: PRO_X_WIRELESS_GHUB_DESCENDING,
    peak_region_mv: 4255,
};

const PRO_X_WIRELESS_GHUB_ASCENDING: &[(u16, u16)] = &[
    (3444, 19),
    (3532, 45),
    (3590, 72),
    (3635, 99),
    (3670, 126),
    (3700, 154),
    (3726, 182),
    (3749, 209),
    (3770, 238),
    (3789, 266),
    (3806, 294),
    (3822, 323),
    (3836, 352),
    (3850, 381),
    (3862, 409),
    (3874, 438),
    (3885, 467),
    (3895, 496),
    (3903, 526),
    (3910, 555),
    (3915, 584),
    (3918, 613),
    (3921, 643),
    (3923, 672),
    (3925, 701),
    (3926, 731),
    (3928, 760),
    (3929, 790),
    (3931, 819),
    (3932, 849),
    (3934, 878),
    (3935, 908),
    (3937, 937),
    (3939, 967),
    (3940, 996),
    (3942, 1026),
    (3944, 1055),
    (3946, 1085),
    (3948, 1115),
    (3949, 1144),
    (3951, 1174),
    (3953, 1203),
    (3955, 1233),
    (3957, 1263),
    (3959, 1292),
    (3961, 1322),
    (3963, 1352),
    (3965, 1382),
    (3967, 1411),
    (3969, 1441),
    (3971, 1471),
    (3973, 1501),
    (3975, 1530),
    (3976, 1560),
    (3978, 1590),
    (3980, 1620),
    (3982, 1650),
    (3983, 1680),
    (3985, 1710),
    (3987, 1739),
    (3989, 1769),
    (3990, 1799),
    (3992, 1829),
    (3993, 1859),
    (3995, 1889),
    (3996, 1919),
    (3998, 1949),
    (4000, 1979),
    (4001, 2009),
    (4002, 2039),
    (4004, 2069),
    (4005, 2099),
    (4006, 2129),
    (4008, 2159),
    (4009, 2189),
    (4010, 2219),
    (4011, 2249),
    (4012, 2279),
    (4013, 2309),
    (4014, 2340),
    (4015, 2370),
    (4016, 2400),
    (4017, 2430),
    (4017, 2460),
    (4018, 2490),
    (4019, 2520),
    (4020, 2550),
    (4020, 2581),
    (4021, 2611),
    (4022, 2641),
    (4022, 2671),
    (4023, 2701),
    (4024, 2731),
    (4024, 2762),
    (4025, 2792),
    (4025, 2822),
    (4026, 2852),
    (4027, 2882),
    (4027, 2913),
    (4028, 2943),
    (4028, 2973),
    (4029, 3003),
    (4029, 3033),
    (4030, 3064),
    (4030, 3094),
    (4031, 3124),
    (4031, 3154),
    (4032, 3184),
    (4033, 3215),
    (4033, 3245),
    (4034, 3275),
    (4034, 3305),
    (4035, 3336),
    (4035, 3366),
    (4036, 3396),
    (4037, 3426),
    (4037, 3457),
    (4038, 3487),
    (4038, 3517),
    (4039, 3548),
    (4039, 3578),
    (4040, 3608),
    (4041, 3638),
    (4041, 3669),
    (4042, 3699),
    (4043, 3729),
    (4043, 3760),
    (4044, 3790),
    (4044, 3820),
    (4045, 3851),
    (4046, 3881),
    (4047, 3911),
    (4047, 3942),
    (4048, 3972),
    (4049, 4002),
    (4049, 4033),
    (4050, 4063),
    (4051, 4094),
    (4052, 4124),
    (4052, 4154),
    (4053, 4185),
    (4054, 4215),
    (4055, 4246),
    (4055, 4276),
    (4056, 4306),
    (4057, 4337),
    (4058, 4367),
    (4059, 4398),
    (4060, 4428),
    (4061, 4459),
    (4061, 4489),
    (4062, 4519),
    (4063, 4550),
    (4064, 4580),
    (4065, 4611),
    (4066, 4641),
    (4067, 4672),
    (4068, 4702),
    (4069, 4733),
    (4070, 4763),
    (4071, 4794),
    (4072, 4824),
    (4073, 4855),
    (4074, 4885),
    (4075, 4916),
    (4076, 4947),
    (4077, 4977),
    (4078, 5008),
    (4079, 5038),
    (4080, 5069),
    (4081, 5099),
    (4082, 5130),
    (4083, 5161),
    (4084, 5191),
    (4085, 5222),
    (4086, 5253),
    (4087, 5283),
    (4089, 5314),
    (4090, 5345),
    (4091, 5375),
    (4092, 5406),
    (4093, 5437),
    (4094, 5467),
    (4096, 5498),
    (4097, 5529),
    (4098, 5559),
    (4099, 5590),
    (4100, 5621),
    (4102, 5651),
    (4103, 5682),
    (4104, 5713),
    (4105, 5743),
    (4106, 5774),
    (4108, 5805),
    (4109, 5836),
    (4110, 5866),
    (4112, 5897),
    (4113, 5928),
    (4114, 5959),
    (4115, 5989),
    (4117, 6020),
    (4118, 6051),
    (4119, 6082),
    (4121, 6113),
    (4122, 6143),
    (4123, 6174),
    (4125, 6205),
    (4126, 6236),
    (4128, 6267),
    (4129, 6298),
    (4130, 6329),
    (4132, 6360),
    (4133, 6390),
    (4135, 6421),
    (4136, 6452),
    (4138, 6483),
    (4139, 6514),
    (4141, 6545),
    (4142, 6576),
    (4144, 6607),
    (4145, 6638),
    (4147, 6669),
    (4148, 6700),
    (4150, 6731),
    (4151, 6762),
    (4153, 6793),
    (4154, 6825),
    (4156, 6856),
    (4158, 6887),
    (4159, 6918),
    (4161, 6949),
    (4163, 6981),
    (4164, 7012),
    (4166, 7043),
    (4168, 7074),
    (4169, 7106),
    (4171, 7137),
    (4173, 7168),
    (4175, 7199),
    (4176, 7231),
    (4178, 7262),
    (4180, 7293),
    (4182, 7325),
    (4184, 7356),
    (4185, 7388),
    (4187, 7419),
    (4189, 7450),
    (4191, 7482),
    (4193, 7513),
    (4195, 7545),
    (4197, 7576),
    (4199, 7608),
    (4201, 7639),
    (4202, 7671),
    (4204, 7702),
    (4206, 7734),
    (4208, 7765),
    (4210, 7797),
    (4212, 7828),
    (4214, 7860),
    (4216, 7891),
    (4218, 7923),
    (4220, 7954),
    (4222, 7986),
    (4224, 8018),
    (4226, 8049),
    (4228, 8081),
    (4230, 8113),
    (4232, 8145),
    (4234, 8176),
    (4236, 8208),
    (4238, 8240),
    (4241, 8272),
    (4243, 8303),
    (4245, 8335),
    (4248, 8367),
    (4250, 8399),
    (4253, 8431),
    (4255, 8463),
    (4258, 8494),
    (4261, 8526),
    (4263, 8558),
    (4266, 8590),
    (4269, 8622),
    (4272, 8654),
    (4275, 8686),
];

const PRO_X_WIRELESS_GHUB_DESCENDING: &[(u16, u16)] = &[
    (4273, 8718),
    (4271, 8748),
    (4271, 8779),
    (4264, 8808),
    (4265, 8837),
    (4265, 8865),
    (4263, 8893),
    (4262, 8920),
    (4257, 8946),
    (4258, 8972),
    (4257, 8997),
    (4257, 9022),
    (4254, 9047),
    (4250, 9071),
    (4251, 9094),
    (4249, 9117),
    (4249, 9140),
    (4247, 9162),
    (4248, 9184),
    (4242, 9206),
    (4243, 9227),
    (4243, 9247),
    (4241, 9267),
    (4241, 9287),
    (4241, 9307),
    (4241, 9326),
    (4238, 9346),
    (4234, 9364),
    (4234, 9382),
    (4234, 9400),
    (4234, 9418),
    (4234, 9435),
    (4232, 9452),
    (4232, 9469),
    (4232, 9486),
    (4232, 9502),
    (4227, 9518),
    (4227, 9533),
    (4227, 9549),
    (4227, 9564),
    (4227, 9579),
    (4225, 9594),
    (4224, 9608),
    (4225, 9622),
    (4224, 9637),
    (4224, 9651),
    (4223, 9664),
    (4222, 9677),
    (4219, 9690),
    (4219, 9703),
    (4219, 9715),
    (4219, 9728),
    (4219, 9740),
    (4219, 9752),
    (4219, 9764),
    (4218, 9776),
    (4218, 9787),
    (4218, 9799),
    (4218, 9810),
    (4214, 9821),
    (4212, 9831),
    (4212, 9842),
    (4212, 9852),
    (4213, 9862),
    (4211, 9872),
    (4212, 9882),
    (4212, 9891),
    (4211, 9901),
    (4210, 9910),
    (4210, 9919),
    (4210, 9928),
    (4209, 9937),
    (4209, 9946),
    (4212, 9955),
    (4209, 9964),
    (4209, 9972),
    (4207, 9980),
    (4206, 9988),
    (4208, 9996),
];

impl VoltageCurve {
    fn points(self) -> &'static [(u16, u8)] {
        match self {
            Self::SolaarLiIon => SOLAAR_LIION,
            Self::HeadsetControlG633 => HEADSETCONTROL_G633,
            Self::ProXWirelessGhub => PRO_X_WIRELESS_GHUB,
        }
    }

    fn charging(self) -> Option<&'static ChargingCurve> {
        match self {
            Self::ProXWirelessGhub => Some(&PRO_X_WIRELESS_GHUB_CHARGING),
            Self::SolaarLiIon | Self::HeadsetControlG633 => None,
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

/// SoC for `mv` on an ascending table: linear between the bracketing points, the first of equal
/// voltages, the top above the table and 0 below it.
fn ascending_soc(points: &[(u16, u16)], mv: u16) -> u16 {
    let Some(i) = points.iter().position(|&(v, _)| v >= mv) else {
        return points.last().map_or(0, |&(_, soc)| soc);
    };
    let (v_hi, soc_hi) = points[i];
    if v_hi == mv {
        return soc_hi;
    }
    let Some(&(v_lo, soc_lo)) = i.checked_sub(1).and_then(|j| points.get(j)) else {
        return 0;
    };
    let soc = u32::from(soc_lo)
        + u32::from(soc_hi - soc_lo) * u32::from(mv - v_lo) / u32::from(v_hi - v_lo);
    u16::try_from(soc).unwrap_or(soc_hi)
}

/// SoC for `mv` on a topping table: where the voltage first fell to `mv` (the table's running
/// minimum, linear between its steps), clamped to the table's ends.
fn topping_soc(points: &[(u16, u16)], mv: u16) -> u16 {
    let Some((&(mut v_hi, mut soc_lo), rest)) = points.split_first() else {
        return 0;
    };
    if mv >= v_hi {
        return soc_lo;
    }
    for &(v, soc) in rest {
        if v >= v_hi {
            continue;
        }
        if mv >= v {
            let soc = u32::from(soc_lo)
                + u32::from(soc - soc_lo) * u32::from(v_hi - mv) / u32::from(v_hi - v);
            return u16::try_from(soc).unwrap_or(soc_lo);
        }
        (v_hi, soc_lo) = (v, soc);
    }
    soc_lo
}

/// What a device has shown since it started charging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChargeTrack {
    /// Highest millivolts since charging started.
    peak_mv: u16,
    /// The voltage fell after reaching the peak region: the constant-voltage phase.
    topping: bool,
}

impl ChargeTrack {
    /// Battery percent for `mv` with status `charge` on `curve`, truncated. A curve with a
    /// charging table reads Charging on its ascending branch until the voltage has reached the
    /// peak region and fallen, then on its descending branch, and Full as 100. Everything else
    /// reads the discharge curve and ends the charge.
    pub fn percent(&mut self, curve: VoltageCurve, mv: u16, charge: Charge) -> u8 {
        let Some(table) = curve.charging() else {
            self.reset();
            return percent(curve, mv);
        };
        match charge {
            Charge::Charging => {
                if mv > self.peak_mv {
                    self.peak_mv = mv;
                } else if mv < self.peak_mv && self.peak_mv >= table.peak_region_mv {
                    self.topping = true;
                }
                let soc = if self.topping {
                    topping_soc(table.descending, mv)
                } else {
                    ascending_soc(table.ascending, mv)
                };
                u8::try_from((soc / 100).min(100)).unwrap_or(100)
            }
            Charge::Full => {
                self.reset();
                100
            }
            _ => {
                self.reset();
                percent(curve, mv)
            }
        }
    }

    /// Forgets the charge, e.g. when the device goes off.
    pub fn reset(&mut self) {
        *self = Self::default();
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

    /// Percents of a charging `curve` fed `readings` (mV, status) in order from fresh state.
    fn track(curve: VoltageCurve, readings: &[(u16, Charge)]) -> Vec<u8> {
        let mut t = ChargeTrack::default();
        readings
            .iter()
            .map(|&(mv, charge)| t.percent(curve, mv, charge))
            .collect()
    }

    #[test]
    fn prox_charging_uses_ascending_curve() {
        use Charge::Charging;
        let prox = VoltageCurve::ProXWirelessGhub;
        // XML ascending point 4.085 V -> 52.219 %: the owner's Refresh reading (98 % on the
        // discharge curve).
        assert_eq!(track(prox, &[(4085, Charging)]), [52]);
        // G HUB showed 58.6631393 % for 4110 mV charging: XML point 4.110 V -> 58.663 %.
        assert_eq!(track(prox, &[(4110, Charging)]), [58]);
        // 4.052 V appears twice (41.24 %, 41.54 %): the first, lower SoC.
        assert_eq!(track(prox, &[(4052, Charging)]), [41]);
        // Between 3.700 V -> 1.537 % and 3.726 V -> 1.815 %: 1.687 %.
        assert_eq!(track(prox, &[(3714, Charging)]), [1]);
        // Above the ascending table (top 4.275 V -> 86.861 %) clamps to its top.
        assert_eq!(track(prox, &[(4300, Charging)]), [86]);
        // Below it: empty.
        assert_eq!(track(prox, &[(3400, Charging)]), [0]);
    }

    #[test]
    fn prox_topping_uses_descending_curve() {
        use Charge::Charging;
        // 4258 mV (ascending 4.258 V -> 84.945 %) is the peak and lies in the peak region; the
        // fall to 4232 mV starts topping: descending 4.232 V -> 94.521 %, then 4.214 V -> 98.211 %.
        assert_eq!(
            track(
                VoltageCurve::ProXWirelessGhub,
                &[
                    (4085, Charging),
                    (4258, Charging),
                    (4232, Charging),
                    (4214, Charging)
                ]
            ),
            [52, 84, 94, 98]
        );
    }

    #[test]
    fn prox_start_mid_charge_reads_ascending_until_a_fall_from_the_peak() {
        use Charge::Charging;
        // No peak seen yet: 4232 mV on the ascending curve (XML point 4.232 V -> 81.445 %), lower
        // than the descending 94 %.
        assert_eq!(
            track(
                VoltageCurve::ProXWirelessGhub,
                &[(4232, Charging), (4258, Charging), (4232, Charging)]
            ),
            [81, 84, 94]
        );
    }

    #[test]
    fn prox_topping_stays_until_charging_stops() {
        use Charge::{Charging, Discharging};
        // A rise during topping stays on the descending curve (above its top 4.273 V -> 87.176 %
        // clamps); a fall that never left the ascending region does not start topping.
        assert_eq!(
            track(
                VoltageCurve::ProXWirelessGhub,
                &[
                    (4300, Charging),
                    (4290, Charging),
                    (4300, Charging),
                    (4100, Discharging),
                    (4232, Charging),
                    (4200, Charging),
                ]
            ),
            [86, 87, 87, 99, 81, 76]
        );
    }

    #[test]
    fn prox_full_is_100_and_discharging_uses_discharge_curve() {
        assert_eq!(
            track(
                VoltageCurve::ProXWirelessGhub,
                &[(4085, Charge::Discharging), (4085, Charge::Full)]
            ),
            [98, 100]
        );
    }

    #[test]
    fn curves_without_charging_table_keep_discharge_curve_while_charging() {
        for curve in [VoltageCurve::HeadsetControlG633, VoltageCurve::SolaarLiIon] {
            for charge in [Charge::Charging, Charge::Full] {
                assert_eq!(
                    track(curve, &[(4000, charge)]),
                    [percent(curve, 4000)],
                    "{curve:?} {charge:?}"
                );
            }
        }
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
