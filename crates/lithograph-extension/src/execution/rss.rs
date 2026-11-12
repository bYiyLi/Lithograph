//! Optional process-lifetime RSS high-water mark at the shared JSON boundary.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn process_peak_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is writable, correctly aligned storage for the native
    // rusage ABI. RUSAGE_SELF requests only this process and retains no pointer.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: the storage started zeroed and getrusage succeeded. Its native
    // struct consists of integer/timeval fields; reading ru_maxrss is initialized.
    let usage = unsafe { usage.assume_init() };
    checked_peak_rss_bytes(usage.ru_maxrss)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn checked_peak_rss_bytes(value: libc::c_long) -> Option<u64> {
    let value = u64::try_from(value).ok()?;
    // Linux getrusage reports KiB. Darwin returns resident_size_max in bytes.
    // https://man7.org/linux/man-pages/man2/getrusage.2.html
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_resource.c
    let bytes_per_unit = if cfg!(target_os = "linux") { 1024 } else { 1 };
    value.checked_mul(bytes_per_unit)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn process_peak_rss_bytes() -> Option<u64> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn native_rss_units_are_checked_and_negative_values_are_unavailable() {
        let factor = if cfg!(target_os = "linux") { 1024 } else { 1 };
        assert_eq!(checked_peak_rss_bytes(7), Some(7 * factor));
        assert_eq!(checked_peak_rss_bytes(-1), None);
        assert_eq!(checked_peak_rss_bytes(0), Some(0));
        let expected = u64::try_from(libc::c_long::MAX)
            .ok()
            .and_then(|value| value.checked_mul(factor));
        assert_eq!(checked_peak_rss_bytes(libc::c_long::MAX), expected);
    }

    #[test]
    fn live_process_rss_high_water_mark_is_available_and_positive() {
        assert!(process_peak_rss_bytes().is_some_and(|bytes| bytes > 0));
    }
}
