//! One row per controller when a pad moves between Bluetooth and a cable.
//!
//! An Xbox pad on a cable leaves Bluetooth at once, and rejoins it on its own within
//! seconds of being unplugged. Its Bluetooth row (`bt:`) and its cable row (`pad:`) share
//! no container, product ID or address, so they are paired by timing alone: a controller
//! row of one family going offline and one of the other family coming online within
//! [`WINDOW_MS`] of each other stand for one pad. Each arrival pairs with at most one
//! departure, the departed row is hidden for the rest of its grace, and an online row is
//! never hidden. Rows only; no device is touched.

use std::collections::{BTreeMap, BTreeSet};

use batterysight_core::model::{DeviceKey, DeviceKind, DeviceState, Presence};
use batterysight_core::time::Ms;

use crate::system::{bluetooth, xbox};

/// How far apart a departure and an arrival may be published. On the owner's pad the
/// Bluetooth row appeared about 6 s after unplugging; a 4 s connect follow-up can add to it.
const WINDOW_MS: Ms = 15_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Bluetooth,
    Cable,
}

fn family(state: &DeviceState) -> Option<Family> {
    if bluetooth::container_of(&state.key).is_some() && state.kind == DeviceKind::Controller {
        Some(Family::Bluetooth)
    } else if xbox::is_pad_key(&state.key) {
        Some(Family::Cable)
    } else {
        None
    }
}

struct Event {
    key: DeviceKey,
    family: Family,
    at: Ms,
}

/// Runtime state of the pairing, updated at each publish.
#[derive(Default)]
pub(crate) struct Handover {
    /// Controller rows online at the last publish.
    online: BTreeMap<DeviceKey, Family>,
    /// Unpaired departures and arrivals inside the window.
    departed: Vec<Event>,
    arrived: Vec<Event>,
    /// Departed rows a successor stands for; hidden while not online.
    hidden: BTreeSet<DeviceKey>,
}

impl Handover {
    /// Records the transitions in `states` (as about to be published at `now`), pairs
    /// them one-to-one, and removes the paired departed rows.
    pub(crate) fn apply(&mut self, states: &mut Vec<DeviceState>, now: Ms) {
        let recent = |event: &Event| now.saturating_sub(event.at) <= WINDOW_MS;
        self.departed.retain(recent);
        self.arrived.retain(recent);
        let online: BTreeMap<DeviceKey, Family> = states
            .iter()
            .filter(|state| state.presence == Presence::Online)
            .filter_map(|state| Some((state.key.clone(), family(state)?)))
            .collect();
        for (key, &family) in &self.online {
            if !online.contains_key(key) {
                self.departed.push(Event {
                    key: key.clone(),
                    family,
                    at: now,
                });
            }
        }
        for (key, &family) in &online {
            if !self.online.contains_key(key) {
                // Back online: neither hidden nor a pending departure any more.
                self.hidden.remove(key);
                self.departed.retain(|event| event.key != *key);
                self.arrived.push(Event {
                    key: key.clone(),
                    family,
                    at: now,
                });
            }
        }
        self.online = online;
        let mut index = 0;
        while index < self.arrived.len() {
            let family = self.arrived[index].family;
            if let Some(departed) = self
                .departed
                .iter()
                .position(|event| event.family != family)
            {
                let departed = self.departed.remove(departed);
                self.arrived.remove(index);
                self.hidden.insert(departed.key);
            } else {
                index += 1;
            }
        }
        self.hidden
            .retain(|key| states.iter().any(|state| state.key == *key));
        states.retain(|state| {
            state.presence == Presence::Online || !self.hidden.contains(&state.key)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batterysight_core::model::{
        Charge, DeviceKey, DeviceKind, DeviceState, Level, Presence, Source, Verification, Via,
    };

    const BT: &str = "bt:0123456789ABCDEF0123456789ABCDEF";
    const BT_2: &str = "bt:FEDCBA9876543210FEDCBA9876543210";
    const USB: &str = "pad:wgi:1";
    const USB_2: &str = "pad:wgi:2";

    fn row(key: &str, kind: DeviceKind, presence: Presence) -> DeviceState {
        DeviceState {
            key: DeviceKey(key.into()),
            name: "Example Controller".into(),
            kind,
            via: if key.starts_with("bt:") {
                Via::Bluetooth
            } else {
                Via::Usb
            },
            level: Some(Level::Percent(64)),
            millivolts: None,
            charge: Charge::Unknown,
            presence,
            updated_ms: 0,
            source: Source::Os,
            verification: Verification::SourceVerified,
            poll_min_ms: None,
            reports_changes: true,
        }
    }

    fn pad(key: &str, presence: Presence) -> DeviceState {
        row(key, DeviceKind::Controller, presence)
    }

    /// Publishes `states` at `now` and returns the visible rows with their presence.
    fn publish(
        handover: &mut Handover,
        mut states: Vec<DeviceState>,
        now: Ms,
    ) -> Vec<(String, Presence)> {
        handover.apply(&mut states, now);
        states
            .into_iter()
            .map(|state| (state.key.0, state.presence))
            .collect()
    }

    fn visible(rows: &[(&str, Presence)]) -> Vec<(String, Presence)> {
        rows.iter()
            .map(|&(key, presence)| (key.to_owned(), presence))
            .collect()
    }

    use Presence::{Offline, Online};

    #[test]
    fn bluetooth_to_usb_shows_one_row_throughout() {
        let mut handover = Handover::default();
        assert_eq!(
            publish(&mut handover, vec![pad(BT, Online)], 0),
            visible(&[(BT, Online)])
        );
        // Plugging the cable: the USB row may be published first...
        assert_eq!(
            publish(
                &mut handover,
                vec![pad(BT, Online), pad(USB, Online)],
                10_000
            ),
            visible(&[(BT, Online), (USB, Online)])
        );
        // ...and the Bluetooth departure seconds later is hidden at once, for its grace.
        for now in [14_000, 40_000, 73_000] {
            assert_eq!(
                publish(&mut handover, vec![pad(BT, Offline), pad(USB, Online)], now),
                visible(&[(USB, Online)])
            );
        }
        assert_eq!(
            publish(&mut handover, vec![pad(USB, Online)], 74_000),
            visible(&[(USB, Online)])
        );
    }

    #[test]
    fn usb_to_bluetooth_shows_one_row_throughout() {
        let mut handover = Handover::default();
        publish(&mut handover, vec![pad(BT, Offline), pad(USB, Online)], 0);
        // Unplugging: the USB row departs first and stays visible until a successor...
        assert_eq!(
            publish(&mut handover, vec![pad(USB, Offline)], 100_000),
            visible(&[(USB, Offline)])
        );
        // ...the pad rejoins Bluetooth about 6 s later, and the USB row goes at once.
        for now in [106_000, 130_000, 159_000] {
            assert_eq!(
                publish(&mut handover, vec![pad(BT, Online), pad(USB, Offline)], now),
                visible(&[(BT, Online)])
            );
        }
    }

    #[test]
    fn a_departure_without_a_successor_keeps_its_grace() {
        let mut handover = Handover::default();
        publish(&mut handover, vec![pad(BT, Online)], 0);
        for now in [5_000, 30_000, 64_000] {
            assert_eq!(
                publish(&mut handover, vec![pad(BT, Offline)], now),
                visible(&[(BT, Offline)])
            );
        }
        // A Bluetooth headset arriving is not a controller and pairs with nothing.
        let headset = row(BT_2, DeviceKind::Headset, Online);
        assert_eq!(
            publish(&mut handover, vec![pad(BT, Offline), headset], 6_000),
            visible(&[(BT, Offline), (BT_2, Online)])
        );
    }

    #[test]
    fn a_usb_pad_arriving_long_after_the_bluetooth_one_left_is_not_paired() {
        let mut handover = Handover::default();
        publish(&mut handover, vec![pad(BT, Online)], 0);
        publish(&mut handover, vec![pad(BT, Offline)], 1_000);
        assert_eq!(
            publish(
                &mut handover,
                vec![pad(BT, Offline), pad(USB, Online)],
                40_000
            ),
            visible(&[(BT, Offline), (USB, Online)])
        );
        // And the reverse: a Bluetooth pad arriving long after a USB pad left.
        let mut handover = Handover::default();
        publish(&mut handover, vec![pad(USB, Online)], 0);
        publish(&mut handover, vec![pad(USB, Offline)], 1_000);
        assert_eq!(
            publish(
                &mut handover,
                vec![pad(BT, Online), pad(USB, Offline)],
                40_000
            ),
            visible(&[(BT, Online), (USB, Offline)])
        );
    }

    #[test]
    fn simultaneous_switches_pair_one_to_one() {
        let mut handover = Handover::default();
        publish(&mut handover, vec![pad(BT, Online), pad(BT_2, Online)], 0);
        // Two pads leave Bluetooth, but only one cable pad arrives: one row stays in grace.
        let rows = publish(
            &mut handover,
            vec![pad(BT, Offline), pad(BT_2, Offline), pad(USB, Online)],
            2_000,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&(USB.to_owned(), Online)));
        // The second cable pad takes the other departed row; neither online row is hidden.
        assert_eq!(
            publish(
                &mut handover,
                vec![
                    pad(BT, Offline),
                    pad(BT_2, Offline),
                    pad(USB, Online),
                    pad(USB_2, Online)
                ],
                3_000,
            ),
            visible(&[(USB, Online), (USB_2, Online)])
        );
        // A departed row that comes back online is shown again, and stays unpaired.
        assert_eq!(
            publish(
                &mut handover,
                vec![
                    pad(BT, Online),
                    pad(BT_2, Offline),
                    pad(USB, Online),
                    pad(USB_2, Online)
                ],
                4_000,
            ),
            visible(&[(BT, Online), (USB, Online), (USB_2, Online)])
        );
    }
}
