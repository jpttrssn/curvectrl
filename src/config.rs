// SPDX-License-Identifier: MPL-2.0

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};

/// User configuration: the film-roll directories shown in the library.
///
/// Version bumped from 1 when the placeholder `demo` key gave way to `rolls`,
/// so existing config files (which only carry `demo`) are not treated as
/// carrying a valid roll list.
#[derive(Debug, Default, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 2]
pub struct Config {
    /// Absolute paths to the roll directories the library lists.
    pub rolls: Vec<String>,
}
