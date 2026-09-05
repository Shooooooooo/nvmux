//! Shared by the integration test files.
//!
//! Both suites need something the machine may not have — a usable `nvim`, an
//! ssh host that answers — and skip rather than fail without it, so a laptop
//! without one still runs the rest. In CI a skip is a silent no-op, which is
//! why `$NVMUX_TEST_REQUIRE` exists: a comma-separated list of what must be
//! present (`nvim`, `ssh`), turning the skip for each into a failure.

#![allow(dead_code)]

/// Whether `$NVMUX_TEST_REQUIRE` names this requirement.
pub fn required(what: &str) -> bool {
    std::env::var("NVMUX_TEST_REQUIRE")
        .map(|v| v.split(',').any(|w| w.trim() == what))
        .unwrap_or(false)
}

/// Skip the test — or fail it, when `$NVMUX_TEST_REQUIRE` names `$what` — if
/// `$available` is false.
#[macro_export]
macro_rules! require {
    ($what:literal, $available:expr, $why:expr) => {
        if !$available {
            if $crate::common::required($what) {
                panic!("NVMUX_TEST_REQUIRE names {} but {}", $what, $why);
            }
            eprintln!("skipping: {}", $why);
            return;
        }
    };
}

/// Names every test session distinctly, so a failed run cannot poison the next.
pub fn unique(tag: &str) -> String {
    format!("it-{tag}-{}", std::process::id())
}
