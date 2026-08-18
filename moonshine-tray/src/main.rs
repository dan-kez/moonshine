//! System tray indicator for the moonshine streaming server.
//!
//! Moonshine has no GUI, so on a desktop host there is no way to tell at a glance whether
//! the daemon is up, whether it is answering, or whether someone is streaming. This sits
//! in the tray as a crescent whose colour reflects the current state, and offers the three
//! things one would otherwise reach for `systemctl` and `journalctl` to do: start, stop or
//! restart the unit, and follow its log.
//!
//! It is a separate binary rather than a mode of the daemon because it belongs to the
//! desktop session, not to the service: it runs as a systemd *user* unit under
//! `graphical-session.target`, while moonshine itself is a system unit.

mod config;
mod icon;
mod probe;
mod unit;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use ksni::menu::StandardItem;
use ksni::{Handle, Icon, MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::probe::State;
use crate::unit::{Systemd, Verb};

/// How often the daemon is polled.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How long to give systemd to act before repainting after a menu action.
///
/// Without this the tray would keep showing the old state until the next tick, which reads
/// as the menu item having done nothing.
const SETTLE_DELAY: Duration = Duration::from_secs(2);

/// How many log lines to show when opening the journal.
const LOG_LINES: &str = "200";

/// Terminals tried in order when `$TERMINAL` is unset or fails to start.
///
/// `xdg-terminal-exec` is the freedesktop-blessed way to ask for "the user's terminal" and
/// comes first; the rest are a pragmatic fallback for desktops that do not ship it.
const TERMINALS: &[&str] = &[
	"xdg-terminal-exec",
	"konsole",
	"ptyxis",
	"gnome-terminal",
	"foot",
	"alacritty",
	"kitty",
	"xterm",
];

#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
	/// Path to the configuration file, read to discover the webserver port.
	config: Option<PathBuf>,

	/// Port to probe, overriding the one in the configuration file.
	#[arg(long)]
	port: Option<u16>,

	/// User whose moonshine instance to track. Defaults to the current user.
	#[arg(long)]
	user: Option<String>,

	/// Systemd unit to control, for deployments that do not use the moonshine@ template.
	#[arg(long, conflicts_with = "user")]
	unit: Option<String>,
}

/// Something the user asked for from the menu.
///
/// Menu callbacks must not block - the menu is frozen until they return - so they only
/// post one of these to the event loop.
#[derive(Debug, PartialEq, Eq)]
enum Action {
	/// Apply a systemd verb to the moonshine unit.
	Unit(Verb),
	/// Open the pairing page in a browser.
	OpenPairingPage,
	/// Open the unit's journal in a terminal.
	ViewLog,
	/// Re-poll now rather than waiting out the interval.
	Refresh,
	/// Shut the tray down.
	Quit,
}

/// The tray item itself: what is currently displayed, and where menu clicks go.
struct MoonshineTray {
	state: State,
	detail: String,
	actions: UnboundedSender<Action>,
}

impl MoonshineTray {
	/// Post an action to the event loop, ignoring the error if it has already stopped.
	fn post(&self, action: Action) {
		let _ = self.actions.send(action);
	}
}

impl Tray for MoonshineTray {
	fn id(&self) -> String {
		"moonshine-tray".to_string()
	}

	fn title(&self) -> String {
		"Moonshine".to_string()
	}

	fn icon_pixmap(&self) -> Vec<Icon> {
		icon::crescent(self.state.colour())
	}

	fn tool_tip(&self) -> ToolTip {
		ToolTip {
			title: format!("Moonshine: {}", self.state.label()),
			description: self.detail.clone(),
			..Default::default()
		}
	}

	fn menu(&self) -> Vec<MenuItem<Self>> {
		let running = self.state != State::Stopped;

		vec![
			StandardItem {
				label: self.state.label().to_string(),
				enabled: false,
				..Default::default()
			}
			.into(),
			MenuItem::Separator,
			StandardItem {
				label: if running { "Stop moonshine" } else { "Start moonshine" }.to_string(),
				activate: Box::new(move |tray: &mut Self| {
					tray.post(Action::Unit(if running { Verb::Stop } else { Verb::Start }));
				}),
				..Default::default()
			}
			.into(),
			StandardItem {
				label: "Restart moonshine".to_string(),
				activate: Box::new(|tray: &mut Self| tray.post(Action::Unit(Verb::Restart))),
				..Default::default()
			}
			.into(),
			MenuItem::Separator,
			StandardItem {
				label: "Pair a client…".to_string(),
				// The page is served by the daemon, so there is nothing to open when it
				// is not answering.
				enabled: matches!(self.state, State::Idle | State::Streaming),
				activate: Box::new(|tray: &mut Self| tray.post(Action::OpenPairingPage)),
				..Default::default()
			}
			.into(),
			StandardItem {
				label: "View log…".to_string(),
				activate: Box::new(|tray: &mut Self| tray.post(Action::ViewLog)),
				..Default::default()
			}
			.into(),
			MenuItem::Separator,
			StandardItem {
				label: "Quit tray".to_string(),
				activate: Box::new(|tray: &mut Self| tray.post(Action::Quit)),
				..Default::default()
			}
			.into(),
		]
	}
}

#[tokio::main]
async fn main() -> ExitCode {
	let args = Args::parse();
	init_tracing();

	match run(args).await {
		Ok(()) => ExitCode::SUCCESS,
		Err(()) => ExitCode::FAILURE,
	}
}

async fn run(args: Args) -> Result<(), ()> {
	let port = args
		.port
		.unwrap_or_else(|| config::webserver_port(&args.config.unwrap_or_else(config::default_config_path)));
	let unit = args
		.unit
		.unwrap_or_else(|| unit::unit_name(&args.user.unwrap_or_else(unit::current_user)));
	tracing::info!("Watching {unit} on port {port}.");

	let systemd = Systemd::connect().await?;
	let (actions, mut incoming) = mpsc::unbounded_channel();

	let handle = MoonshineTray {
		state: State::Stopped,
		detail: "Checking…".to_string(),
		actions: actions.clone(),
	}
	.spawn()
	.await
	.map_err(|e| tracing::error!("Failed to register a tray icon: {e}"))?;

	// Paint the real state immediately rather than showing "Checking…" for a full interval.
	refresh(&systemd, &handle, &unit, port).await;

	let mut ticker = tokio::time::interval(POLL_INTERVAL);
	ticker.tick().await;

	loop {
		tokio::select! {
			_ = ticker.tick() => refresh(&systemd, &handle, &unit, port).await,
			_ = tokio::signal::ctrl_c() => break,
			action = incoming.recv() => match action {
				Some(Action::Unit(verb)) => {
					// A failed verb is already logged, and the next poll shows the truth
					// either way, so a refusal at the polkit prompt needs nothing here.
					let _ = systemd.apply(verb, &unit).await;
					schedule_refresh(actions.clone());
				},
				Some(Action::OpenPairingPage) => open_pairing_page(port),
				Some(Action::ViewLog) => view_log(&unit),
				Some(Action::Refresh) => refresh(&systemd, &handle, &unit, port).await,
				Some(Action::Quit) | None => break,
			},
		}
	}

	handle.shutdown().await;

	Ok(())
}

/// Poll the daemon and repaint the tray.
async fn refresh(systemd: &Systemd, handle: &Handle<MoonshineTray>, unit: &str, port: u16) {
	let info = probe::fetch(port).await.ok();

	// systemd is only asked when the daemon did not answer, since that is the only case
	// where the answer changes anything - and it keeps the common path off the system bus.
	let unit_active = match info {
		Some(_) => true,
		None => systemd.is_active(unit).await,
	};

	let (state, detail) = probe::classify(info.as_ref(), unit_active, port);

	handle
		.update(|tray| {
			tray.state = state;
			tray.detail = detail;
		})
		.await;
}

/// Ask the event loop to re-poll once systemd has had a moment to act.
fn schedule_refresh(actions: UnboundedSender<Action>) {
	tokio::spawn(async move {
		tokio::time::sleep(SETTLE_DELAY).await;
		let _ = actions.send(Action::Refresh);
	});
}

/// URL of the daemon's PIN entry page.
///
/// No `uniqueid` is passed, so the page falls back to the same placeholder id the
/// command-line pairing instructions in the README use. The notification moonshine
/// raises on an incoming pairing request links to the id-specific page; this menu item
/// is the way to reach the page without one, having missed or dismissed it.
fn pairing_url(port: u16) -> String {
	format!("http://localhost:{port}/pin")
}

/// Open the pairing page in the user's browser.
///
/// `open::that` blocks until the launcher exits, so it runs on the blocking pool rather
/// than on the event loop. Its detached sibling would not block, but it drops the child
/// handle without waiting, leaving the launcher a zombie for the lifetime of the tray -
/// and unlike the terminal spawned for the journal, there is no handle here to reap.
fn open_pairing_page(port: u16) {
	let url = pairing_url(port);

	tokio::task::spawn_blocking(move || match open::that(&url) {
		Ok(()) => tracing::debug!("Opened the pairing page at {url}."),
		Err(e) => tracing::warn!("Couldn't open the pairing page automatically ({e}). Open it manually: {url}"),
	});
}

/// Open the unit's journal in the user's terminal.
///
/// This is the one place the tray spawns a process. There is no journal viewer that can be
/// handed a URL the way the pairing notification hands one to `open`, and `journalctl -f`
/// needs a terminal to live in.
fn view_log(unit: &str) {
	let candidates = std::env::var("TERMINAL")
		.ok()
		.into_iter()
		.chain(TERMINALS.iter().map(|terminal| terminal.to_string()));

	for terminal in candidates {
		let mut command = tokio::process::Command::new(&terminal);
		command
			.args(execute_flag(&terminal))
			.args(["journalctl", "-u", unit, "-f", "-n", LOG_LINES]);

		match command.spawn() {
			Ok(mut child) => {
				tracing::debug!("Opened the journal in {terminal}.");
				// Reap the terminal when it exits, rather than leaving a zombie behind.
				tokio::spawn(async move {
					let _ = child.wait().await;
				});
				return;
			},
			Err(e) => tracing::debug!("Could not open the journal in {terminal}: {e}"),
		}
	}

	tracing::warn!("Found no terminal to show the journal in. Set $TERMINAL to one.");
}

/// The flag a terminal needs before a command to execute, if any.
fn execute_flag(terminal: &str) -> &'static [&'static str] {
	// Matched on the basename so that an absolute path in $TERMINAL still resolves.
	match terminal.rsplit('/').next().unwrap_or(terminal) {
		"gnome-terminal" | "ptyxis" => &["--"],
		"konsole" | "xterm" | "alacritty" | "kitty" => &["-e"],
		// xdg-terminal-exec and foot take the command as trailing arguments.
		_ => &[],
	}
}

fn init_tracing() {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer().with_ansi(std::io::stdout().is_terminal()))
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("moonshine_tray=info")))
		.init();
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A tray in the given state, plus the receiving end of its action channel.
	fn tray(state: State) -> (MoonshineTray, mpsc::UnboundedReceiver<Action>) {
		let (actions, incoming) = mpsc::unbounded_channel();
		(
			MoonshineTray {
				state,
				detail: "some detail".to_string(),
				actions,
			},
			incoming,
		)
	}

	/// Label and enabled flag of every non-separator entry, in menu order.
	fn entries(tray: &MoonshineTray) -> Vec<(String, bool)> {
		tray.menu()
			.into_iter()
			.filter_map(|item| match item {
				MenuItem::Standard(item) => Some((item.label, item.enabled)),
				_ => None,
			})
			.collect()
	}

	/// Activate the entry with the given label, returning what it posted.
	fn activate(state: State, label: &str) -> Option<Action> {
		let (mut tray, mut incoming) = tray(state);
		let callback = tray
			.menu()
			.into_iter()
			.find_map(|item| match item {
				MenuItem::Standard(item) if item.label == label => Some(item.activate),
				_ => None,
			})
			.unwrap_or_else(|| panic!("no menu entry labelled {label:?}"));

		callback(&mut tray);
		incoming.try_recv().ok()
	}

	#[test]
	fn the_menu_lists_every_action_in_order() {
		let (tray, _incoming) = tray(State::Idle);
		let labels: Vec<String> = entries(&tray).into_iter().map(|(label, _)| label).collect();
		assert_eq!(
			labels,
			vec![
				"Idle — waiting for a client",
				"Stop moonshine",
				"Restart moonshine",
				"Pair a client…",
				"View log…",
				"Quit tray",
			]
		);
	}

	#[test]
	fn the_status_entry_is_never_clickable() {
		for state in [State::Stopped, State::Starting, State::Idle, State::Streaming] {
			let (tray, _incoming) = tray(state);
			let (label, enabled) = entries(&tray)[0].clone();
			assert_eq!(label, state.label());
			assert!(!enabled, "status entry clickable in {state:?}");
		}
	}

	#[test]
	fn the_toggle_follows_whether_the_unit_is_running() {
		let (stopped, _a) = tray(State::Stopped);
		let (idle, _b) = tray(State::Idle);
		assert!(entries(&stopped).iter().any(|(label, _)| label == "Start moonshine"));
		assert!(entries(&idle).iter().any(|(label, _)| label == "Stop moonshine"));
	}

	#[test]
	fn the_toggle_posts_the_verb_matching_its_label() {
		assert_eq!(
			activate(State::Stopped, "Start moonshine"),
			Some(Action::Unit(Verb::Start))
		);
		assert_eq!(activate(State::Idle, "Stop moonshine"), Some(Action::Unit(Verb::Stop)));
	}

	#[test]
	fn the_other_entries_post_their_own_actions() {
		assert_eq!(
			activate(State::Idle, "Restart moonshine"),
			Some(Action::Unit(Verb::Restart))
		);
		assert_eq!(activate(State::Idle, "Pair a client…"), Some(Action::OpenPairingPage));
		assert_eq!(activate(State::Idle, "View log…"), Some(Action::ViewLog));
		assert_eq!(activate(State::Idle, "Quit tray"), Some(Action::Quit));
	}

	#[test]
	fn pairing_is_offered_only_while_the_daemon_answers() {
		let offered = |state| {
			let (tray, _incoming) = tray(state);
			entries(&tray)
				.into_iter()
				.find(|(label, _)| label == "Pair a client…")
				.expect("pairing entry missing")
				.1
		};

		assert!(offered(State::Idle));
		assert!(offered(State::Streaming));
		assert!(!offered(State::Stopped));
		assert!(!offered(State::Starting));
	}

	#[test]
	fn the_icon_and_tooltip_follow_the_state() {
		let (tray, _incoming) = tray(State::Streaming);
		assert_eq!(tray.id(), "moonshine-tray");
		assert_eq!(tray.title(), "Moonshine");

		let tool_tip = tray.tool_tip();
		assert!(tool_tip.title.contains("Streaming"));
		assert_eq!(tool_tip.description, "some detail");

		// ksni::Icon carries no PartialEq, so compare the fields that matter.
		let rendered = tray.icon_pixmap();
		let expected = icon::crescent(State::Streaming.colour());
		assert_eq!(rendered.len(), expected.len());
		for (rendered, expected) in rendered.iter().zip(expected.iter()) {
			assert_eq!(
				(rendered.width, rendered.height, &rendered.data),
				(expected.width, expected.height, &expected.data)
			);
		}
	}

	#[test]
	fn terminals_that_need_a_separator_get_the_right_one() {
		assert_eq!(execute_flag("gnome-terminal"), &["--"]);
		assert_eq!(execute_flag("konsole"), &["-e"]);
	}

	#[test]
	fn terminals_taking_a_trailing_command_get_no_separator() {
		assert!(execute_flag("foot").is_empty());
		assert!(execute_flag("xdg-terminal-exec").is_empty());
	}

	#[test]
	fn the_pairing_url_points_at_the_configured_port() {
		assert_eq!(pairing_url(47989), "http://localhost:47989/pin");
		assert_eq!(pairing_url(48000), "http://localhost:48000/pin");
	}

	#[test]
	fn an_absolute_terminal_path_is_matched_on_its_basename() {
		assert_eq!(execute_flag("/usr/bin/konsole"), &["-e"]);
	}
}
