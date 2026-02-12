use std::sync::OnceLock;
use std::time::Instant;

static MONO_START: OnceLock<Instant> = OnceLock::new();

#[inline]
pub fn monotonic_now_us() -> u64 {
    MONO_START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[inline]
pub fn monotonic_now_secs() -> u64 {
    MONO_START.get_or_init(Instant::now).elapsed().as_secs()
}

#[inline]
pub fn monotonic_now_ns() -> u64 {
    MONO_START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn monotonic_now_us_increases() {
        let t1 = monotonic_now_us();
        std::thread::sleep(Duration::from_millis(2));
        let t2 = monotonic_now_us();
        assert!(t2 > t1);
    }

    #[test]
    fn monotonic_now_ns_non_decreasing() {
        let t1 = monotonic_now_ns();
        let t2 = monotonic_now_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn monotonic_now_secs_non_decreasing() {
        let t1 = monotonic_now_secs();
        let t2 = monotonic_now_secs();
        assert!(t2 >= t1);
    }
}
