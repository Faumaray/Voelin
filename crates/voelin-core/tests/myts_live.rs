//! Opt-in account + isolated server interoperability check. No keys are saved.
//! Requires VOELIN_MYTS_LIVE=1, EMAIL/PASSWORD/DEVICE_ID and optional OTP under
//! the VOELIN_MYTS_ prefix; TEST_TS3/TEST_TS6 and QUERY_TS3/QUERY_TS6 give
//! loopback voice/query addresses. QUERY_PASSWORD authenticates local query.
#![cfg(not(target_os = "android"))]

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use tokio::{sync::broadcast, time::timeout};
use voelin_core::{Command, Engine, Event, VoiceOptions, VoiceState};
use voelin_myts::{Client, ServerIdentity};
use voelin_query::{Command as QueryCommand, QueryClient, Row};

fn env(name: &str) -> Result<String> {
	std::env::var(format!("VOELIN_MYTS_{name}")).with_context(|| format!("missing {name}"))
}

async fn connected(events: &mut broadcast::Receiver<Event>, session_id: u64) -> Result<u16> {
	timeout(Duration::from_secs(35), async {
		loop {
			match events.recv().await? {
				Event::State { session, state }
					if session == session_id && state.voice == VoiceState::Connected =>
				{
					if let Some(id) = state.own_client {
						return Ok(id);
					}
				}
				Event::Error { message, .. } => bail!("engine: {message}"),
				_ => {}
			}
		}
	})
	.await?
}

async fn info(query: &QueryClient, client: u16) -> Result<Row> {
	query
		.send(&QueryCommand::new("clientinfo").arg("clid", client))
		.await?
		.into_iter()
		.next()
		.context("clientinfo missing")
}

async fn wait_id(query: &QueryClient, client: u16, expected: &str) -> Result<()> {
	timeout(Duration::from_secs(12), async {
		loop {
			let row = info(query, client).await?;
			if row.get("client_myteamspeak_id") == Some(expected) {
				return Ok(());
			}
			tokio::time::sleep(Duration::from_millis(200)).await;
		}
	})
	.await
	.context("server did not report the expected myTS association")?
}

async fn server(identity: Arc<ServerIdentity>, kind: &str) -> Result<()> {
	let address = env(&format!("TEST_{kind}"))?;
	let query_address = env(&format!("QUERY_{kind}"))?;
	// This test is deliberately limited to caller-owned loopback test servers.
	ensure!(address.starts_with("127.0.0.1:"), "voice endpoint must be loopback");
	ensure!(query_address.starts_with("127.0.0.1:"), "query endpoint must be loopback");
	let (query, _) = QueryClient::connect(&voelin_query::Connect {
		transport: if kind == "TS3" {
			voelin_query::Transport::Raw
		} else {
			voelin_query::Transport::Ssh
		},
		addr: query_address,
		user: "serveradmin".into(),
		secret: Some(env("QUERY_PASSWORD")?),
		server_id: Some(1),
		server_port: None,
		line: voelin_query::LineOptions { rate_limit: None, ..Default::default() },
	})
	.await?;
	let engine = Engine::start();
	let mut events = engine.subscribe();
	let result = async {
		engine.send(Command::SetMytsIdentity(Some(identity.clone())));
		for session in 1..=2 {
			engine.send(Command::ConnectVoice {
				session,
				options: Box::new(VoiceOptions::new(&address, format!("myts-proof-{session}"))),
			});
			let client = connected(&mut events, session).await?;
			let row = info(&query, client).await?;
			let version = voelin_core::versions::native_version()?;
			ensure!(row.get("client_platform") == Some(version.get_platform()), "wrong platform");
			ensure!(
				row.get("client_version") == Some(version.get_version_string()),
				"wrong version"
			);
			let metadata: serde_json::Value =
				serde_json::from_str(row.get("client_meta_data").context("missing metadata")?)?;
			ensure!(metadata["name"] == "Voelin", "wrong client metadata");
			ensure!(metadata["platform"] == std::env::consts::OS, "wrong metadata platform");
			eprintln!("{kind}: native platform/Voelin metadata passed");
			wait_id(&query, client, &identity.id()).await?;
			eprintln!("{kind}: signed clientinit myTS ID passed");
			// Two fresh connections prove the signature is bound to each handshake.
			engine.send(Command::SetMytsIdentity(None));
			wait_id(&query, client, "").await?;
			engine.send(Command::SetMytsIdentity(Some(identity.clone())));
			wait_id(&query, client, &identity.id()).await?;
			eprintln!("{kind}: live clear and reattach passed");
			engine.send(Command::CloseSession { session });
		}
		Ok(())
	}
	.await;
	engine.send(Command::SetMytsIdentity(None));
	for session in 1..=2 {
		engine.send(Command::CloseSession { session });
	}
	query.quit().await;
	result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires owner-authorized account and isolated loopback servers"]
async fn authenticated_account_and_server_lifecycle() -> Result<()> {
	ensure!(env("LIVE")? == "1", "explicit live opt-in required");
	let client = Client::new()?;
	let login = client
		.login(&env("EMAIL")?, &env("PASSWORD")?, env("OTP").ok().as_deref(), &env("DEVICE_ID")?)
		.await?;
	// Capture every result before deleting the newly created remote session.
	let result = async {
		client.validate_session(&login.token).await?;
		let identity = Arc::new(login.identity.context("server identity unlock failed")?);
		let mut failures = Vec::new();
		for kind in ["TS3", "TS6"] {
			if let Err(error) = server(identity.clone(), kind).await {
				failures.push(format!("{kind}: {error:#}"));
			}
		}
		ensure!(failures.is_empty(), "{}", failures.join("; "));
		Ok(())
	}
	.await;
	let logout = client.logout(&login.token).await;
	logout.context("remote test session revocation failed")?;
	result
}
