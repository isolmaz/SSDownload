//! The hold: while a guard is on in the settings and Windows reports its condition (a metered
//! connection, Battery saver), no download starts and running ones go back to the queue.

use super::*;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use windows_sys::Win32::{
    NetworkManagement::IpHelper::GetNetworkConnectivityHint,
    Networking::WinSock::{
        NetworkConnectivityCostHintFixed, NetworkConnectivityCostHintVariable,
        NL_NETWORK_CONNECTIVITY_HINT,
    },
    System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
};

/// How often the sensors are read again while the guards stay as they are.
pub(super) const HOLD_POLL: Duration = Duration::from_secs(15);

/// What the machine reports. The engine asks Windows; tests answer for it.
pub(super) trait Sensors: Send {
    fn connection_is_metered(&mut self) -> bool;
    fn battery_saver_is_on(&mut self) -> bool;
}

pub(super) struct WindowsSensors;

impl Sensors for WindowsSensors {
    fn connection_is_metered(&mut self) -> bool {
        let mut hint: NL_NETWORK_CONNECTIVITY_HINT = unsafe { std::mem::zeroed() };
        let error = unsafe { GetNetworkConnectivityHint(&mut hint) };
        error == 0 && hint_is_metered(&hint)
    }

    /// `SystemStatusFlag` is 1 while Battery saver is on, at whatever charge level the user
    /// chose in Windows for it to start.
    fn battery_saver_is_on(&mut self) -> bool {
        let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        let read = unsafe { GetSystemPowerStatus(&mut status) } != 0;
        read && status.SystemStatusFlag == 1
    }
}

/// A fixed or variable cost, roaming and a used-up data allowance each mean a metered connection.
fn hint_is_metered(hint: &NL_NETWORK_CONNECTIVITY_HINT) -> bool {
    hint.ConnectivityCost == NetworkConnectivityCostHintFixed
        || hint.ConnectivityCost == NetworkConnectivityCostHintVariable
        || hint.Roaming != 0
        || hint.OverDataLimit != 0
}

/// A sensor is read only for a guard that is on.
pub(super) fn hold_for(settings: &Settings, sensors: &mut dyn Sensors) -> Hold {
    Hold {
        metered: settings.pause_on_metered && sensors.connection_is_metered(),
        battery_saver: settings.pause_on_battery_saver && sensors.battery_saver_is_on(),
    }
}

/// Keeps the last reading between polls.
pub(super) struct HoldMonitor {
    sensors: Box<dyn Sensors>,
    interval: Duration,
    /// Which guards were on at the last reading, and when it was taken.
    read: Option<((bool, bool), Instant)>,
    hold: Hold,
}

impl HoldMonitor {
    pub(super) fn new(sensors: Box<dyn Sensors>, interval: Duration) -> Self {
        Self {
            sensors,
            interval,
            read: None,
            hold: Hold::default(),
        }
    }

    /// The hold now. The sensors are read at the first call, when `interval` has passed since the
    /// last reading and when a guard was switched on or off since then.
    pub(super) fn current(&mut self, settings: &Settings) -> Hold {
        let guards = (settings.pause_on_metered, settings.pause_on_battery_saver);
        if self
            .read
            .is_none_or(|(read_for, at)| read_for != guards || at.elapsed() >= self.interval)
        {
            self.hold = hold_for(settings, self.sensors.as_mut());
            self.read = Some((guards, Instant::now()));
        }
        self.hold
    }
}

/// Sensors a test sets by hand; each read of a sensor is counted.
#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct FakeSensors {
    pub(super) metered: Arc<AtomicBool>,
    pub(super) battery_saver: Arc<AtomicBool>,
    pub(super) metered_reads: Arc<AtomicUsize>,
    pub(super) battery_saver_reads: Arc<AtomicUsize>,
}

#[cfg(test)]
impl Sensors for FakeSensors {
    fn connection_is_metered(&mut self) -> bool {
        self.metered_reads.fetch_add(1, Ordering::AcqRel);
        self.metered.load(Ordering::Acquire)
    }

    fn battery_saver_is_on(&mut self) -> bool {
        self.battery_saver_reads.fetch_add(1, Ordering::AcqRel);
        self.battery_saver.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Networking::WinSock::{
        NetworkConnectivityCostHintUnknown, NetworkConnectivityCostHintUnrestricted,
    };

    fn guards(metered: bool, battery_saver: bool) -> Settings {
        Settings {
            pause_on_metered: metered,
            pause_on_battery_saver: battery_saver,
            ..Settings::default()
        }
    }

    fn sensors(metered: bool, battery_saver: bool) -> FakeSensors {
        let sensors = FakeSensors::default();
        sensors.metered.store(metered, Ordering::Release);
        sensors
            .battery_saver
            .store(battery_saver, Ordering::Release);
        sensors
    }

    fn reads(counter: &Arc<AtomicUsize>) -> usize {
        counter.load(Ordering::Acquire)
    }

    #[test]
    fn a_guard_holds_only_while_it_is_on_and_its_condition_is_reported() {
        // (metered guard, battery saver guard, metered, battery saver) -> hold
        for (settings, metered, saver, expected) in [
            ((false, false), true, true, (false, false)),
            ((true, false), true, true, (true, false)),
            ((false, true), true, true, (false, true)),
            ((true, true), true, true, (true, true)),
            ((true, true), false, true, (false, true)),
            ((true, true), true, false, (true, false)),
            ((true, true), false, false, (false, false)),
            ((true, false), false, true, (false, false)),
            ((false, true), true, false, (false, false)),
        ] {
            let hold = hold_for(
                &guards(settings.0, settings.1),
                &mut sensors(metered, saver),
            );
            assert_eq!(
                (hold.metered, hold.battery_saver),
                expected,
                "guards {settings:?}, metered {metered}, battery saver {saver}"
            );
            assert_eq!(hold.is_active(), expected.0 || expected.1);
        }
    }

    #[test]
    fn a_sensor_is_not_read_for_a_guard_that_is_off() {
        let fake = sensors(true, true);
        hold_for(&guards(true, false), &mut fake.clone());
        assert_eq!(
            (reads(&fake.metered_reads), reads(&fake.battery_saver_reads)),
            (1, 0)
        );
        hold_for(&guards(false, true), &mut fake.clone());
        assert_eq!(
            (reads(&fake.metered_reads), reads(&fake.battery_saver_reads)),
            (1, 1)
        );
    }

    #[test]
    fn the_monitor_keeps_its_reading_until_the_interval_passes() {
        let fake = sensors(true, false);
        let mut monitor = HoldMonitor::new(Box::new(fake.clone()), Duration::from_secs(3600));
        let settings = guards(true, false);
        assert!(monitor.current(&settings).metered);

        fake.metered.store(false, Ordering::Release);
        assert!(
            monitor.current(&settings).metered,
            "kept within the interval"
        );
        assert_eq!(reads(&fake.metered_reads), 1);

        let mut monitor = HoldMonitor::new(Box::new(fake.clone()), Duration::ZERO);
        fake.metered.store(true, Ordering::Release);
        assert!(monitor.current(&settings).metered);
        fake.metered.store(false, Ordering::Release);
        assert!(!monitor.current(&settings).metered, "read again once due");
    }

    #[test]
    fn switching_a_guard_on_reads_its_sensor_at_once() {
        let fake = sensors(false, true);
        let mut monitor = HoldMonitor::new(Box::new(fake.clone()), Duration::from_secs(3600));
        assert!(!monitor.current(&guards(false, false)).is_active());
        assert_eq!(reads(&fake.battery_saver_reads), 0);

        assert!(monitor.current(&guards(false, true)).battery_saver);
        assert_eq!(reads(&fake.battery_saver_reads), 1);

        assert!(
            !monitor.current(&guards(false, false)).is_active(),
            "switching the guard off lifts the hold at once"
        );
    }

    fn hint(cost: i32, roaming: u8, over_data_limit: u8) -> NL_NETWORK_CONNECTIVITY_HINT {
        let mut hint: NL_NETWORK_CONNECTIVITY_HINT = unsafe { std::mem::zeroed() };
        hint.ConnectivityCost = cost;
        hint.Roaming = roaming;
        hint.OverDataLimit = over_data_limit;
        hint
    }

    #[test]
    fn windows_cost_hints_are_metered_when_the_cost_is_fixed_or_variable_or_a_limit_is_hit() {
        for (hint, metered) in [
            (hint(NetworkConnectivityCostHintUnknown, 0, 0), false),
            (hint(NetworkConnectivityCostHintUnrestricted, 0, 0), false),
            (hint(NetworkConnectivityCostHintFixed, 0, 0), true),
            (hint(NetworkConnectivityCostHintVariable, 0, 0), true),
            (hint(NetworkConnectivityCostHintUnrestricted, 1, 0), true),
            (hint(NetworkConnectivityCostHintUnrestricted, 0, 1), true),
        ] {
            assert_eq!(
                hint_is_metered(&hint),
                metered,
                "cost {} roaming {} over the limit {}",
                hint.ConnectivityCost,
                hint.Roaming,
                hint.OverDataLimit
            );
        }
    }
}
