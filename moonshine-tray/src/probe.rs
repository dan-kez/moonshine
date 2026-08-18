//! Polling moonshine's own GameStream endpoint to determine what the daemon is doing.
//!
//! The tray deliberately reads state from `/serverinfo` rather than from systemd alone.
//! A daemon that systemd reports as `active` may still be unresponsive - during startup,
//! or because it has hung - and reporting that as "running" would be misleading. Systemd
//! is consulted only to tell "stopped" apart from "not answering yet".

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

/// How long to wait for the daemon to answer before treating it as unreachable.
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

/// The `state` value moonshine reports while a client is streaming.
///
/// See the `/serverinfo` handler in `moonshine-core/src/webserver/mod.rs`, which emits
/// `MOONSHINE_SERVER_BUSY` whenever a session context exists and `MOONSHINE_SERVER_FREE`
/// otherwise. Note this is moonshine's own vocabulary, not GFE's `SUNSHINE_SERVER_BUSY`.
const STATE_BUSY: &str = "MOONSHINE_SERVER_BUSY";

/// What the tray displays, in increasing order of "the daemon is doing something useful".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
	/// The systemd unit is not running.
	Stopped,
	/// The unit is up, but the daemon is not answering on its webserver port yet.
	Starting,
	/// The daemon is answering and waiting for a Moonlight client.
	Idle,
	/// A client has launched an application.
	Streaming,
}

impl State {
	/// Icon colour for this state, as `0xRRGGBB`.
	pub fn colour(self) -> u32 {
		match self {
			State::Stopped => 0x6e6e6e,
			State::Starting => 0xe0a030,
			State::Idle => 0x3fb950,
			State::Streaming => 0x3d8bfd,
		}
	}

	/// Short human-readable label, shown as the first (disabled) menu entry.
	pub fn label(self) -> &'static str {
		match self {
			State::Stopped => "Stopped",
			State::Starting => "Starting…",
			State::Idle => "Idle — waiting for a client",
			State::Streaming => "Streaming",
		}
	}
}

/// The subset of `/serverinfo` the tray cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
	/// The name moonshine advertises to clients.
	pub hostname: String,
	/// Whether a session is currently active.
	pub streaming: bool,
	/// Id of the running application, or 0 when idle.
	pub current_game: i32,
}

/// Fetch and parse `/serverinfo`, or `Err(())` if the daemon did not answer in time.
///
/// Failures here are expected and routine (the daemon is stopped, or still starting), so
/// they are logged at debug level rather than as warnings.
pub async fn fetch(port: u16) -> Result<ServerInfo, ()> {
	let response = tokio::time::timeout(HTTP_TIMEOUT, request(port))
		.await
		.map_err(|_| tracing::debug!("Timed out waiting for /serverinfo on port {port}."))??;

	parse_server_info(&response)
}

/// Perform the plain HTTP GET against the loopback webserver.
async fn request(port: u16) -> Result<String, ()> {
	let stream = TcpStream::connect(("127.0.0.1", port))
		.await
		.map_err(|e| tracing::debug!("Failed to connect to moonshine on port {port}: {e}"))?;

	let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
		.await
		.map_err(|e| tracing::debug!("Failed HTTP handshake with moonshine: {e}"))?;

	// The connection future drives the socket and completes once `sender` is dropped.
	tokio::spawn(async move {
		let _ = connection.await;
	});

	// The uniqueid is a dummy: it only affects PairStatus, which this tray never reads.
	let request = Request::builder()
		.uri("/serverinfo?uniqueid=0")
		.header(hyper::header::HOST, "localhost")
		.body(Empty::<Bytes>::new())
		.map_err(|e| tracing::warn!("Failed to build /serverinfo request: {e}"))?;

	let response = sender
		.send_request(request)
		.await
		.map_err(|e| tracing::debug!("Failed to send /serverinfo request: {e}"))?;

	let body = response
		.into_body()
		.collect()
		.await
		.map_err(|e| tracing::debug!("Failed to read /serverinfo response: {e}"))?
		.to_bytes();

	Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Parse the fields the tray needs out of a `/serverinfo` response body.
///
/// Moonshine builds this XML by string concatenation and emits no prolog, attributes or
/// namespaces on the elements we read, so matching tags directly is sufficient and avoids
/// pulling an XML parser into the dependency tree. A missing `state` element means we are
/// not talking to moonshine, and is treated as a failed probe.
pub fn parse_server_info(body: &str) -> Result<ServerInfo, ()> {
	let state =
		extract_tag(body, "state").ok_or_else(|| tracing::debug!("No <state> element in the /serverinfo response."))?;

	Ok(ServerInfo {
		hostname: extract_tag(body, "hostname").unwrap_or_else(|| "Moonshine".to_string()),
		streaming: state == STATE_BUSY,
		current_game: extract_tag(body, "currentgame")
			.and_then(|game| game.parse().ok())
			.unwrap_or(0),
	})
}

/// Return the unescaped text content of the first `<tag>…</tag>` in `body`.
fn extract_tag(body: &str, tag: &str) -> Option<String> {
	let open = format!("<{tag}>");
	let close = format!("</{tag}>");

	let start = body.find(&open)? + open.len();
	let end = body[start..].find(&close)? + start;

	Some(unescape_xml(&body[start..end]))
}

/// Reverse of the `escape_xml` moonshine applies to text it embeds in `/serverinfo`.
///
/// `&amp;` is substituted last so that an escaped entity in the original text (`&amp;lt;`
/// on the wire) survives as literal `&lt;` rather than being unescaped twice.
fn unescape_xml(input: &str) -> String {
	input
		.replace("&lt;", "<")
		.replace("&gt;", ">")
		.replace("&quot;", "\"")
		.replace("&apos;", "'")
		.replace("&amp;", "&")
}

/// Turn a probe result into the state to display and a line of tooltip detail.
///
/// `unit_active` is only consulted when the daemon did not answer, to distinguish a unit
/// that is starting up (or wedged) from one that was never started.
pub fn classify(info: Option<&ServerInfo>, unit_active: bool, port: u16) -> (State, String) {
	match info {
		Some(info) if info.streaming => {
			let application = if info.current_game != 0 {
				format!("app {}", info.current_game)
			} else {
				"an application".to_string()
			};
			(State::Streaming, format!("{} — streaming {application}", info.hostname))
		},
		Some(info) => (State::Idle, format!("{} — ready on port {port}", info.hostname)),
		None if unit_active => (
			State::Starting,
			format!("Unit is active, but not answering on port {port}"),
		),
		None => (State::Stopped, "Unit is not running".to_string()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A response shaped exactly like the one `moonshine-core`'s webserver emits.
	fn serverinfo(state: &str, current_game: &str, hostname: &str) -> String {
		format!(
			"<root status_code=\"200\"><hostname>{hostname}</hostname><appversion>7.1.431.-1</appversion>\
			 <HttpsPort>47984</HttpsPort><PairStatus>0</PairStatus>\
			 <currentgame>{current_game}</currentgame><state>{state}</state></root>"
		)
	}

	#[test]
	fn free_server_parses_as_not_streaming() {
		let info = parse_server_info(&serverinfo("MOONSHINE_SERVER_FREE", "0", "Moonshine")).unwrap();
		assert!(!info.streaming);
		assert_eq!(info.current_game, 0);
		assert_eq!(info.hostname, "Moonshine");
	}

	#[test]
	fn busy_server_parses_as_streaming_with_application_id() {
		let info = parse_server_info(&serverinfo("MOONSHINE_SERVER_BUSY", "881448767", "Desktop")).unwrap();
		assert!(info.streaming);
		assert_eq!(info.current_game, 881448767);
	}

	#[test]
	fn escaped_hostname_is_unescaped() {
		let info = parse_server_info(&serverinfo("MOONSHINE_SERVER_FREE", "0", "Ben &amp; Jerry&apos;s")).unwrap();
		assert_eq!(info.hostname, "Ben & Jerry's");
	}

	#[test]
	fn double_escaped_entity_is_unescaped_only_once() {
		let info = parse_server_info(&serverinfo("MOONSHINE_SERVER_FREE", "0", "&amp;lt;tag&amp;gt;")).unwrap();
		assert_eq!(info.hostname, "&lt;tag&gt;");
	}

	#[test]
	fn missing_hostname_falls_back_to_a_default() {
		let info = parse_server_info("<root><state>MOONSHINE_SERVER_FREE</state></root>").unwrap();
		assert_eq!(info.hostname, "Moonshine");
		assert_eq!(info.current_game, 0);
	}

	#[test]
	fn response_without_state_is_rejected() {
		assert!(parse_server_info("<root><hostname>Moonshine</hostname></root>").is_err());
		assert!(parse_server_info("not xml at all").is_err());
	}

	#[test]
	fn unterminated_tag_is_rejected() {
		assert!(parse_server_info("<root><state>MOONSHINE_SERVER_FREE</root>").is_err());
	}

	/// Serve one canned HTTP response on an ephemeral port, and return that port.
	///
	/// A plain blocking listener on its own thread, so the test needs no extra tokio
	/// features to stand up the other end of the socket.
	fn serve_once(body: &'static str) -> u16 {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();

		std::thread::spawn(move || {
			use std::io::{Read, Write};

			let (mut stream, _) = listener.accept().unwrap();
			// Drain the request first, so the client is not writing into a closing socket.
			let _ = stream.read(&mut [0u8; 1024]);
			let response = format!(
				"HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{body}",
				body.len()
			);
			let _ = stream.write_all(response.as_bytes());
		});

		port
	}

	#[tokio::test]
	async fn fetch_reads_and_parses_a_real_response() {
		let port = serve_once(
			"<root status_code=\"200\"><hostname>Front Room</hostname>\
			 <currentgame>7</currentgame><state>MOONSHINE_SERVER_BUSY</state></root>",
		);

		let info = fetch(port).await.unwrap();
		assert_eq!(info.hostname, "Front Room");
		assert!(info.streaming);
		assert_eq!(info.current_game, 7);
	}

	#[tokio::test]
	async fn fetch_fails_when_nothing_is_listening() {
		// Bind and drop, so the port is one nothing can answer on.
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();
		drop(listener);

		assert!(fetch(port).await.is_err());
	}

	#[tokio::test]
	async fn fetch_rejects_a_response_that_is_not_serverinfo() {
		let port = serve_once("<root status_code=\"200\"><hostname>Front Room</hostname></root>");
		assert!(fetch(port).await.is_err());
	}

	#[test]
	fn unreachable_daemon_with_active_unit_is_starting() {
		let (state, detail) = classify(None, true, 47989);
		assert_eq!(state, State::Starting);
		assert!(detail.contains("47989"));
	}

	#[test]
	fn unreachable_daemon_with_inactive_unit_is_stopped() {
		assert_eq!(classify(None, false, 47989).0, State::Stopped);
	}

	#[test]
	fn answering_daemon_is_idle_regardless_of_unit_state() {
		let info = ServerInfo {
			hostname: "Moonshine".to_string(),
			streaming: false,
			current_game: 0,
		};
		assert_eq!(classify(Some(&info), false, 47989).0, State::Idle);
	}

	#[test]
	fn streaming_without_an_application_id_still_reads_as_streaming() {
		let info = ServerInfo {
			hostname: "Moonshine".to_string(),
			streaming: true,
			current_game: 0,
		};
		let (state, detail) = classify(Some(&info), true, 47989);
		assert_eq!(state, State::Streaming);
		assert!(detail.contains("an application"));
	}

	#[test]
	fn every_state_has_a_distinct_colour() {
		let colours = [State::Stopped, State::Starting, State::Idle, State::Streaming].map(State::colour);
		for (index, colour) in colours.iter().enumerate() {
			assert!(!colours[index + 1..].contains(colour), "duplicate colour {colour:#08x}");
		}
	}
}
