//! Inspecting and controlling `moonshine@<user>.service` over D-Bus.
//!
//! Moonshine installs a *system* unit that runs as the target user, so the tray - running
//! unprivileged in a desktop session - cannot manage it directly. Rather than shelling out
//! to `pkexec systemctl`, the manager methods are called with the D-Bus
//! `ALLOW_INTERACTIVE_AUTHORIZATION` flag set, which lets systemd defer to polkit and the
//! desktop raise its own authentication prompt. No polkit rule is installed and no
//! standing privilege is granted.
//!
//! The call style here (raw `call_method` against string constants rather than a
//! `#[zbus::proxy]` trait) follows `moonshine-core/src/session/application.rs`, which does
//! the same against the session bus.

use zbus::proxy::MethodFlags;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, Proxy};

const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const ACTIVE_STATE_PROPERTY: &str = "ActiveState";

/// Error systemd returns for a unit it has never loaded.
const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";

/// A verb the tray can ask systemd to apply to the moonshine unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
	/// Start the unit.
	Start,
	/// Stop the unit.
	Stop,
	/// Restart the unit.
	Restart,
}

impl Verb {
	/// Name of the `org.freedesktop.systemd1.Manager` method implementing this verb.
	fn method(self) -> &'static str {
		match self {
			Verb::Start => "StartUnit",
			Verb::Stop => "StopUnit",
			Verb::Restart => "RestartUnit",
		}
	}
}

/// Name of the moonshine unit instance belonging to `user`.
pub fn unit_name(user: &str) -> String {
	format!("moonshine@{user}.service")
}

/// The user whose moonshine instance this tray should track.
///
/// Falls back to `LOGNAME` and finally to the literal `%i` placeholder, which will simply
/// fail to resolve and show as stopped rather than silently tracking the wrong instance.
pub fn current_user() -> String {
	std::env::var("USER")
		.or_else(|_| std::env::var("LOGNAME"))
		.unwrap_or_else(|_| "%i".to_string())
}

/// Whether a systemd `ActiveState` value means the unit is running or on its way up.
///
/// `activating` and `reloading` count as running so that a unit which is still starting is
/// reported as such, instead of flickering through "stopped" on every restart.
fn is_running(active_state: &str) -> bool {
	matches!(active_state, "active" | "activating" | "reloading")
}

/// A connection to the system bus, used to talk to systemd's manager.
pub struct Systemd {
	connection: Connection,
	manager: Proxy<'static>,
}

impl Systemd {
	/// Connect to the system bus and bind a proxy to systemd's manager object.
	pub async fn connect() -> Result<Self, ()> {
		let connection = Connection::system()
			.await
			.map_err(|e| tracing::error!("Failed to connect to the system bus: {e}"))?;

		let manager = Proxy::new(&connection, SYSTEMD_BUS, SYSTEMD_PATH, SYSTEMD_MANAGER)
			.await
			.map_err(|e| tracing::error!("Failed to create a systemd proxy: {e}"))?;

		Ok(Self { connection, manager })
	}

	/// Whether the given unit is currently running.
	///
	/// A unit systemd has never heard of, and any error reaching systemd at all, both read
	/// as "not running" - which is what the tray would show anyway, and is only ever used
	/// to add detail to a probe that already failed.
	pub async fn is_active(&self, unit: &str) -> bool {
		let path: OwnedObjectPath = match self.manager.call("GetUnit", &(unit,)).await {
			Ok(path) => path,
			Err(zbus::Error::MethodError(ref name, ..)) if name.as_str() == NO_SUCH_UNIT => return false,
			Err(e) => {
				tracing::debug!("Failed to look up {unit}: {e}");
				return false;
			},
		};

		let reply = match self
			.connection
			.call_method(
				Some(SYSTEMD_BUS),
				&path,
				Some(PROPERTIES_INTERFACE),
				"Get",
				&(UNIT_INTERFACE, ACTIVE_STATE_PROPERTY),
			)
			.await
		{
			Ok(reply) => reply,
			Err(e) => {
				tracing::debug!("Failed to read {ACTIVE_STATE_PROPERTY} of {unit}: {e}");
				return false;
			},
		};

		let active_state = reply
			.body()
			.deserialize::<OwnedValue>()
			.ok()
			.and_then(|state| String::try_from(state).ok());

		match active_state {
			Some(active_state) => is_running(&active_state),
			None => {
				tracing::debug!("Unexpected {ACTIVE_STATE_PROPERTY} reply for {unit}.");
				false
			},
		}
	}

	/// Apply `verb` to `unit`, letting polkit prompt the user if authorisation is needed.
	///
	/// Returns once systemd has *enqueued* the job, not once it has finished; the poller
	/// picks up the result on its next pass.
	pub async fn apply(&self, verb: Verb, unit: &str) -> Result<(), ()> {
		self.manager
			.call_with_flags::<_, _, OwnedObjectPath>(
				verb.method(),
				MethodFlags::AllowInteractiveAuth.into(),
				&(unit, "replace"),
			)
			.await
			.map(|_| ())
			.map_err(|e| tracing::warn!("Failed to {} {unit}: {e}", verb.method()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unit_name_is_an_instance_of_the_moonshine_template() {
		assert_eq!(unit_name("kez"), "moonshine@kez.service");
	}

	#[test]
	fn running_states_include_transitional_ones() {
		assert!(is_running("active"));
		assert!(is_running("activating"));
		assert!(is_running("reloading"));
	}

	#[test]
	fn stopped_states_are_not_running() {
		assert!(!is_running("inactive"));
		assert!(!is_running("failed"));
		assert!(!is_running("deactivating"));
		assert!(!is_running(""));
	}

	#[test]
	fn every_verb_maps_to_a_manager_method() {
		assert_eq!(Verb::Start.method(), "StartUnit");
		assert_eq!(Verb::Stop.method(), "StopUnit");
		assert_eq!(Verb::Restart.method(), "RestartUnit");
	}
}
