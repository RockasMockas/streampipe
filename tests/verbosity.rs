//! Verbosity plumbing shared by the CLI and the GUI.
//!
//! `-v` counts up, `--verbose-max` caps it, and both map onto a `tracing` level
//! plus the periodic-stats flag. `RUST_LOG` overrides everything (not tested here).

use streampipe::common::{log_level, stats_enabled, CommonArgs, MAX_VERBOSE};
use tracing::Level;

fn common(verbose: u8, verbose_max: u8) -> CommonArgs {
    CommonArgs {
        ipv4_addr: None,
        ipv6_addr: None,
        custom_alpn: None,
        verbose,
        verbose_max,
    }
}

#[test]
fn verbosity_defaults_to_quiet() {
    assert_eq!(common(0, MAX_VERBOSE).verbosity(), 0);
    assert_eq!(log_level(0), Level::ERROR);
}

#[test]
fn verbosity_maps_to_levels() {
    assert_eq!(common(1, MAX_VERBOSE).verbosity(), 1);
    assert_eq!(log_level(1), Level::INFO);
    assert_eq!(common(2, MAX_VERBOSE).verbosity(), 2);
    assert_eq!(log_level(2), Level::DEBUG);
    assert_eq!(common(3, MAX_VERBOSE).verbosity(), 3);
    assert_eq!(log_level(3), Level::TRACE);
}

#[test]
fn verbose_max_caps_verbosity() {
    // -v -v -v with --verbose-max 1 is just info
    assert_eq!(common(3, 1).verbosity(), 1);
    assert_eq!(log_level(common(3, 1).verbosity()), Level::INFO);
    // --verbose-max above the maximum is clamped to the maximum
    assert_eq!(common(10, 100).verbosity(), MAX_VERBOSE);
    // --verbose-max 0 mutes everything
    assert_eq!(common(5, 0).verbosity(), 0);
    // a lower verbose count is unaffected by a higher cap
    assert_eq!(common(2, 3).verbosity(), 2);
}

#[test]
fn stats_start_at_debug() {
    assert!(!stats_enabled(0));
    assert!(!stats_enabled(1));
    assert!(stats_enabled(2));
    assert!(stats_enabled(3));
}
