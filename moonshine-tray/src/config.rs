//! Reading the handful of settings the tray shares with the daemon.
//!
//! The tray only needs to know which port to probe, so it reads that one value out of
//! moonshine's config file directly rather than depending on `moonshine-core`. Pulling in
//! the core crate would drag the compositor, Vulkan and encoder stacks into a process
//! whose entire job is to draw a coloured moon.

use std::path::{Path, PathBuf};

/// Port moonshine's HTTP webserver listens on unless configured otherwise.
///
/// Mirrors `WebserverConfig::default()` in `moonshine-core/src/webserver/mod.rs`.
pub const DEFAULT_PORT: u16 = 47989;

/// Path moonshine reads its configuration from when not told otherwise.
///
/// Mirrors `default_config_path()` in `src/main.rs`, so that running the tray with no
/// arguments inspects the same file the daemon does.
pub fn default_config_path() -> PathBuf {
	match std::env::var("XDG_CONFIG_HOME") {
		Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("moonshine/config.toml"),
		_ => {
			let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
			PathBuf::from(home).join(".config/moonshine/config.toml")
		},
	}
}

/// Read `webserver.port` from the config file, falling back to [`DEFAULT_PORT`].
///
/// Every failure here is soft. A missing file is the normal case on a host where the
/// daemon has not run yet, and an unreadable or malformed one is the daemon's problem to
/// report - the tray should still come up and show that moonshine is not answering.
pub fn webserver_port(path: &Path) -> u16 {
	let contents = match std::fs::read_to_string(path) {
		Ok(contents) => contents,
		Err(e) => {
			tracing::debug!("Not reading a port from {}: {e}", path.display());
			return DEFAULT_PORT;
		},
	};

	parse_webserver_port(&contents).unwrap_or_else(|| {
		tracing::debug!("No webserver.port in {}, assuming {DEFAULT_PORT}.", path.display());
		DEFAULT_PORT
	})
}

/// Extract `webserver.port` from the contents of a config file.
fn parse_webserver_port(contents: &str) -> Option<u16> {
	contents
		.parse::<toml::Table>()
		.map_err(|e| tracing::warn!("Failed to parse the moonshine config: {e}"))
		.ok()?
		.get("webserver")?
		.get("port")?
		.as_integer()?
		.try_into()
		.ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_a_configured_port() {
		let contents = "[webserver]\nport = 48000\nport_https = 48001\n";
		assert_eq!(parse_webserver_port(contents), Some(48000));
	}

	#[test]
	fn missing_webserver_section_has_no_port() {
		assert_eq!(parse_webserver_port("[stream]\nfec_percentage = 20\n"), None);
	}

	#[test]
	fn missing_port_key_has_no_port() {
		assert_eq!(parse_webserver_port("[webserver]\nport_https = 47984\n"), None);
	}

	#[test]
	fn malformed_config_has_no_port() {
		assert_eq!(parse_webserver_port("[webserver\nport ="), None);
	}

	#[test]
	fn out_of_range_port_is_rejected() {
		assert_eq!(parse_webserver_port("[webserver]\nport = 70000\n"), None);
	}

	#[test]
	fn unreadable_file_falls_back_to_the_default_port() {
		assert_eq!(
			webserver_port(Path::new("/nonexistent/moonshine/config.toml")),
			DEFAULT_PORT
		);
	}
}
