//! System appearance detection for automatic day/night theming.
//!
//! Uses the `dark-light` crate for cross-platform detection:
//! - macOS: reads `AppleInterfaceStyle` preference
//! - Linux: queries XDG Desktop Portal (`org.freedesktop.appearance.color-scheme`)
//! - Windows: reads system personalization registry
//!
//! Falls back to OSC 11 terminal background query when `dark-light` returns
//! `Unspecified` (e.g., over SSH where no desktop session is available).
//! The OSC 11 fallback is **startup-only** — see [`detect_with_osc11_fallback`].
//!
//! Falls back to `None` on total detection failure.

use super::ThemeKind;
use std::time::Duration;
use tokio::sync::watch;

/// Detected system appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAppearance {
    Light,
    Dark,
}

/// Detect the current system appearance (desktop APIs only).
///
/// Detection chain:
/// 1. `dark-light::detect()` — desktop session APIs (macOS/Linux/Windows)
/// 2. `None` — if detection fails
///
/// For the extended chain that includes OSC 11 as a startup-only fallback,
/// see [`detect_with_osc11_fallback`].
///
/// In `#[cfg(test)]` builds, checks the mock override first so that
/// `SystemAppearanceWatcher`'s polling loop (which calls `detect()`
/// directly) is also controllable from tests.
#[must_use]
pub fn detect() -> Option<SystemAppearance> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(v) = mock_override() {
        return v;
    }

    detect_without_mock()
}

/// Detect system appearance with OSC 11 terminal background fallback.
///
/// Extended detection chain:
/// 1. `dark-light::detect()` — desktop session APIs
/// 2. OSC 11 terminal background query — fallback for SSH/headless
/// 3. `None` — if both fail
///
/// **Startup-only**: the OSC 11 step requires raw-mode stdin access and
/// must NOT be called once crossterm's `EventStream` is active.  The
/// live [`SystemAppearanceWatcher`] uses [`detect`] (without OSC 11).
#[must_use]
pub fn detect_with_osc11_fallback() -> Option<SystemAppearance> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(v) = mock_override() {
        return v;
    }

    detect_without_mock().or_else(super::osc11::detect_via_osc11)
}

/// Inner detection via desktop APIs only (no mock, no OSC 11).
fn detect_without_mock() -> Option<SystemAppearance> {
    match dark_light::detect() {
        Ok(dark_light::Mode::Dark) => Some(SystemAppearance::Dark),
        Ok(dark_light::Mode::Light) => Some(SystemAppearance::Light),
        // Mode::Unspecified or Err — no system preference detected
        _ => None,
    }
}

/// Return the mock value if one has been set (test builds only).
///
/// Returns `Some(value)` when a mock is active, `None` when real
/// detection should proceed.
#[cfg(any(test, feature = "test-support"))]
fn mock_override() -> Option<Option<SystemAppearance>> {
    *MOCK_APPEARANCE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Map system appearance to a theme kind using config-driven overrides.
///
/// `dark_theme` and `light_theme` are the user-configured themes for each
/// appearance mode, read from `[ui].auto_dark_theme` and `[ui].auto_light_theme`
/// in `config.toml`. When `None`, defaults to `GrokNight` / `GrokDay`.
///
/// This function is the single mapping point for appearance -> theme.
/// All callers go through it, making the mapping trivially extensible.
#[must_use]
pub fn to_theme_kind(
    appearance: SystemAppearance,
    dark_theme: Option<ThemeKind>,
    light_theme: Option<ThemeKind>,
) -> ThemeKind {
    match appearance {
        SystemAppearance::Light => light_theme.unwrap_or(ThemeKind::GrokDay),
        SystemAppearance::Dark => dark_theme.unwrap_or(ThemeKind::GrokNight),
    }
}

/// Polling interval for system appearance detection.
///
/// In test builds, a shorter interval (50ms) is used so polling tests
/// complete quickly.
#[cfg(not(test))]
const POLL_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Watches for system appearance changes via polling.
///
/// The spawned polling task only reads system state and sends via
/// `watch::channel` — it never mutates `theme_cache::CURRENT` or `AUTO_MODE`.
/// The watcher does NOT use OSC 11 for polling — only [`detect()`].
pub struct SystemAppearanceWatcher {
    rx: watch::Receiver<Option<SystemAppearance>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl SystemAppearanceWatcher {
    /// Start the watcher if auto mode is active.
    ///
    /// Returns `None` when `is_auto` is false — the event loop uses
    /// `std::future::pending()` in that case so the `select!` branch
    /// never fires.
    pub fn start_if_auto(is_auto: bool) -> Option<Self> {
        if !is_auto {
            return None;
        }

        let initial = detect();
        let (tx, rx) = watch::channel(initial);
        let interval = POLL_INTERVAL;

        let handle = tokio::spawn(async move {
            let mut current = initial;
            loop {
                tokio::time::sleep(interval).await;
                let detected = detect();
                if detected != current {
                    current = detected;
                    let _ = tx.send(current);
                }
            }
        });

        Some(Self {
            rx,
            _handle: handle,
        })
    }

    /// Wait for the next appearance change.
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.rx.changed().await
    }

    /// Return the current detected appearance.
    #[must_use]
    pub fn current(&self) -> Option<SystemAppearance> {
        *self.rx.borrow()
    }
}

impl Drop for SystemAppearanceWatcher {
    fn drop(&mut self) {
        self._handle.abort();
    }
}

// -- Test support ----------------------------------------------------------

#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;

/// Mock override for `detect()`. When set to `Some(value)`, `detect()`
/// returns the mock value instead of calling `dark_light::detect()`.
/// This ensures the `SystemAppearanceWatcher` polling loop (which calls
/// `detect()` directly) is also controllable from tests.
#[cfg(any(test, feature = "test-support"))]
static MOCK_APPEARANCE: Mutex<Option<Option<SystemAppearance>>> = Mutex::new(None);

/// Override `detect()` for tests. Set to `Some(value)` to mock a specific
/// appearance, or `None` to mock detection failure.
#[cfg(any(test, feature = "test-support"))]
pub fn set_mock(value: Option<SystemAppearance>) {
    *MOCK_APPEARANCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
}

/// Clear the mock override, restoring real detection behavior.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_mock() {
    *MOCK_APPEARANCE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::super::cache as theme_cache;
    use super::*;

    /// Helper: set mock, assert `detect()` returns the expected value, clear mock.
    /// Caller must hold `theme_cache::test_lock()` to prevent races with parallel
    /// tests in `cache::tests` and `slash::commands::theme::tests` that also
    /// mutate the shared `MOCK_APPEARANCE` static via `set_mock`/`clear_mock`.
    fn assert_mock_roundtrip(value: Option<SystemAppearance>) {
        set_mock(value);
        assert_eq!(detect(), value);
        clear_mock();
    }

    #[test]
    fn to_theme_kind_dark_defaults_to_groknight() {
        let result = to_theme_kind(SystemAppearance::Dark, None, None);
        assert_eq!(result, ThemeKind::GrokNight);
    }

    #[test]
    fn to_theme_kind_light_defaults_to_grokday() {
        let result = to_theme_kind(SystemAppearance::Light, None, None);
        assert_eq!(result, ThemeKind::GrokDay);
    }

    #[test]
    fn to_theme_kind_custom_dark_theme() {
        let result = to_theme_kind(SystemAppearance::Dark, Some(ThemeKind::TokyoNight), None);
        assert_eq!(result, ThemeKind::TokyoNight);
    }

    #[test]
    fn to_theme_kind_custom_light_theme() {
        let result = to_theme_kind(SystemAppearance::Light, None, Some(ThemeKind::RosePineMoon));
        assert_eq!(result, ThemeKind::RosePineMoon);
    }

    #[test]
    fn to_theme_kind_custom_both() {
        let result = to_theme_kind(
            SystemAppearance::Dark,
            Some(ThemeKind::RosePineMoon),
            Some(ThemeKind::GrokNight),
        );
        assert_eq!(result, ThemeKind::RosePineMoon);

        let result = to_theme_kind(
            SystemAppearance::Light,
            Some(ThemeKind::RosePineMoon),
            Some(ThemeKind::GrokNight),
        );
        assert_eq!(result, ThemeKind::GrokNight);
    }

    #[test]
    fn to_theme_kind_dark_ignores_light_override() {
        let result = to_theme_kind(SystemAppearance::Dark, None, Some(ThemeKind::TokyoNight));
        // Dark appearance should use the dark default, not the light override.
        assert_eq!(result, ThemeKind::GrokNight);
    }

    #[test]
    fn to_theme_kind_light_ignores_dark_override() {
        let result = to_theme_kind(SystemAppearance::Light, Some(ThemeKind::TokyoNight), None);
        // Light appearance should use the light default, not the dark override.
        assert_eq!(result, ThemeKind::GrokDay);
    }

    #[test]
    fn mock_dark_appearance() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_mock_roundtrip(Some(SystemAppearance::Dark));
    }

    #[test]
    fn mock_light_appearance() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_mock_roundtrip(Some(SystemAppearance::Light));
    }

    #[test]
    fn mock_detection_failure() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_mock_roundtrip(None);
    }

    #[test]
    fn clear_mock_restores_real_detection() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_mock(Some(SystemAppearance::Dark));
        assert_eq!(detect(), Some(SystemAppearance::Dark));
        clear_mock();
        // After clearing, detect() calls dark_light::detect() for real.
        // We can't assert a specific value since it depends on the system,
        // but we can verify it doesn't panic.
        let _ = detect();
    }

    // -- SystemAppearanceWatcher -----------------------------------------

    #[tokio::test]
    async fn start_if_auto_returns_none_when_not_auto() {
        assert!(SystemAppearanceWatcher::start_if_auto(false).is_none());
    }

    #[tokio::test]
    async fn start_if_auto_returns_some_when_auto() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_mock(Some(SystemAppearance::Dark));
        let watcher = SystemAppearanceWatcher::start_if_auto(true);
        assert!(watcher.is_some());
        clear_mock();
    }

    #[tokio::test]
    async fn watcher_reports_initial_appearance() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_mock(Some(SystemAppearance::Light));
        let watcher = SystemAppearanceWatcher::start_if_auto(true).unwrap();
        assert_eq!(watcher.current(), Some(SystemAppearance::Light));
        clear_mock();
    }

    #[tokio::test]
    async fn watcher_reports_none_on_detection_failure() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_mock(None);
        let watcher = SystemAppearanceWatcher::start_if_auto(true).unwrap();
        assert_eq!(watcher.current(), None);
        clear_mock();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Deliberate: theme_cache::test_lock() serializes mock access.
    async fn watcher_detects_appearance_change() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_mock(Some(SystemAppearance::Dark));
        let mut watcher = SystemAppearanceWatcher::start_if_auto(true).unwrap();
        assert_eq!(watcher.current(), Some(SystemAppearance::Dark));

        // Change the mock appearance.
        set_mock(Some(SystemAppearance::Light));

        // Wait for the watcher to detect the change (polls every 50ms in tests).
        tokio::time::timeout(std::time::Duration::from_secs(2), watcher.changed())
            .await
            .expect("timed out waiting for change")
            .expect("watcher channel closed");

        assert_eq!(watcher.current(), Some(SystemAppearance::Light));
        clear_mock();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Deliberate: theme_cache::test_lock() serializes mock access.
    async fn watcher_does_not_send_when_unchanged() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_mock(Some(SystemAppearance::Dark));
        let mut watcher = SystemAppearanceWatcher::start_if_auto(true).unwrap();

        // Wait longer than the poll interval — no change should occur.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(200), watcher.changed()).await;

        // Should timeout because appearance didn't change.
        assert!(
            result.is_err(),
            "expected timeout — no change should be emitted"
        );
        assert_eq!(watcher.current(), Some(SystemAppearance::Dark));
        clear_mock();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Deliberate: theme_cache::test_lock() serializes mock access.
    async fn watcher_detects_recovery_from_failure() {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_mock(None); // Initially detection fails
        let mut watcher = SystemAppearanceWatcher::start_if_auto(true).unwrap();
        assert_eq!(watcher.current(), None);

        // Now detection succeeds.
        set_mock(Some(SystemAppearance::Dark));

        tokio::time::timeout(std::time::Duration::from_secs(2), watcher.changed())
            .await
            .expect("timed out waiting for recovery")
            .expect("watcher channel closed");

        assert_eq!(watcher.current(), Some(SystemAppearance::Dark));
        clear_mock();
    }
}
