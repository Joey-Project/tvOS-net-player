use std::time::Duration;

const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SuspendTimestamp(u128);

impl SuspendTimestamp {
    #[cfg(test)]
    pub(crate) const fn from_nanos(nanoseconds: u128) -> Self {
        Self(nanoseconds)
    }

    pub(crate) fn checked_add(self, duration: Duration) -> Option<Self> {
        self.0.checked_add(duration.as_nanos()).map(Self)
    }

    pub(crate) fn checked_elapsed_since(self, earlier: Self) -> Option<Duration> {
        let elapsed = self.0.checked_sub(earlier.0)?;
        let seconds = u64::try_from(elapsed / NANOSECONDS_PER_SECOND).ok()?;
        let nanoseconds = (elapsed % NANOSECONDS_PER_SECOND) as u32;
        Some(Duration::new(seconds, nanoseconds))
    }
}

pub(crate) trait SuspendClock: Send + Sync {
    fn now(&self) -> Option<SuspendTimestamp>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemSuspendClock;

impl SuspendClock for SystemSuspendClock {
    fn now(&self) -> Option<SuspendTimestamp> {
        #[cfg(target_vendor = "apple")]
        {
            apple_now()
        }

        #[cfg(target_os = "linux")]
        {
            linux_now()
        }

        #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
        {
            None
        }
    }
}

#[cfg(target_vendor = "apple")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

#[cfg(target_vendor = "apple")]
#[link(name = "System")]
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

#[cfg(target_vendor = "apple")]
fn apple_now() -> Option<SuspendTimestamp> {
    use std::sync::OnceLock;

    static TIMEBASE: OnceLock<Option<MachTimebaseInfo>> = OnceLock::new();
    let timebase = (*TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        let result = unsafe { mach_timebase_info(&mut info) };
        (result == 0 && info.numer != 0 && info.denom != 0).then_some(info)
    }))?;

    let ticks = unsafe { mach_continuous_time() };
    let nanoseconds = u128::from(ticks)
        .checked_mul(u128::from(timebase.numer))?
        .checked_div(u128::from(timebase.denom))?;
    Some(SuspendTimestamp(nanoseconds))
}

#[cfg(target_os = "linux")]
fn linux_now() -> Option<SuspendTimestamp> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return None;
    }

    let seconds = u128::try_from(time.tv_sec).ok()?;
    let nanoseconds = u32::try_from(time.tv_nsec).ok()?;
    if u128::from(nanoseconds) >= NANOSECONDS_PER_SECOND {
        return None;
    }

    let total_nanoseconds = seconds
        .checked_mul(NANOSECONDS_PER_SECOND)?
        .checked_add(u128::from(nanoseconds))?;
    Some(SuspendTimestamp(total_nanoseconds))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use super::{SuspendClock, SuspendTimestamp};

    #[derive(Default)]
    struct ScriptedClock(Mutex<VecDeque<Option<SuspendTimestamp>>>);

    impl ScriptedClock {
        fn new(readings: impl IntoIterator<Item = Option<SuspendTimestamp>>) -> Self {
            Self(Mutex::new(readings.into_iter().collect()))
        }
    }

    impl SuspendClock for ScriptedClock {
        fn now(&self) -> Option<SuspendTimestamp> {
            self.0.lock().ok()?.pop_front().flatten()
        }
    }

    #[test]
    fn injected_clock_advances_and_elapsed_is_checked() {
        let clock = ScriptedClock::new([
            Some(SuspendTimestamp::from_nanos(4_000_000_000)),
            Some(SuspendTimestamp::from_nanos(9_000_000_000)),
        ]);
        let start = clock.now().unwrap();
        let end = clock.now().unwrap();

        assert_eq!(
            end.checked_elapsed_since(start),
            Some(std::time::Duration::from_secs(5))
        );
    }

    #[test]
    fn backwards_clock_reading_has_no_elapsed_duration() {
        let clock = ScriptedClock::new([
            Some(SuspendTimestamp::from_nanos(20)),
            Some(SuspendTimestamp::from_nanos(19)),
        ]);
        let start = clock.now().unwrap();
        let end = clock.now().unwrap();

        assert_eq!(end.checked_elapsed_since(start), None);
    }

    #[test]
    fn unavailable_clock_reading_stays_unavailable() {
        let clock = ScriptedClock::new([None]);

        assert_eq!(clock.now(), None);
    }

    #[test]
    fn checked_add_rejects_timestamp_overflow() {
        assert_eq!(
            SuspendTimestamp::from_nanos(u128::MAX).checked_add(std::time::Duration::from_nanos(1)),
            None
        );
    }

    #[test]
    fn checked_elapsed_rejects_underflow() {
        assert_eq!(
            SuspendTimestamp::from_nanos(9).checked_elapsed_since(SuspendTimestamp::from_nanos(10)),
            None
        );
    }

    #[test]
    fn checked_elapsed_rejects_duration_conversion_overflow() {
        let seconds_beyond_duration = u128::from(u64::MAX) + 1;
        let elapsed_nanoseconds = seconds_beyond_duration * 1_000_000_000;

        assert_eq!(
            SuspendTimestamp::from_nanos(elapsed_nanoseconds)
                .checked_elapsed_since(SuspendTimestamp::from_nanos(0)),
            None
        );
    }
}
