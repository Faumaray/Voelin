//! SSH transport: password login, then the query runs in the shell channel.

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::PublicKeyOrCertificate;

use crate::tcp::QuickAck;
use crate::{Error, Result};

/// Accepts any host key. ServerQuery hosts generate their key on first start
/// and there is no out-of-band way to learn it; callers that care can pin the
/// fingerprint returned by [`connect`].
pub(crate) struct AcceptAny {
	fingerprint: Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for AcceptAny {
	type Error = russh::Error;

	async fn check_server_key(
		&mut self,
		key: &PublicKeyOrCertificate,
	) -> std::result::Result<bool, Self::Error> {
		if let PublicKeyOrCertificate::PublicKey { key, .. } = key {
			*self.fingerprint.lock().unwrap() =
				Some(key.fingerprint(russh::keys::HashAlg::Sha256).to_string());
		}
		Ok(true)
	}
}

/// An open SSH shell channel. The session handle is kept with it because the
/// connection closes when the handle is dropped.
pub(crate) struct Shell {
	pub stream: russh::ChannelStream<client::Msg>,
	pub session: client::Handle<AcceptAny>,
	pub fingerprint: Option<String>,
}

/// Open an SSH session and start the query shell, giving up on the TCP
/// connection after `timeout`.
pub(crate) async fn connect(
	addr: &str,
	user: &str,
	password: &str,
	timeout: Duration,
) -> Result<Shell> {
	let config = Arc::new(client::Config {
		inactivity_timeout: None,
		keepalive_interval: Some(Duration::from_secs(60)),
		nodelay: true,
		..Default::default()
	});
	let fingerprint = Arc::new(std::sync::Mutex::new(None));
	let handler = AcceptAny { fingerprint: fingerprint.clone() };
	let stream = QuickAck::connect(addr, timeout).await?;
	let mut session = client::connect_stream(config, stream, handler).await?;
	let auth = session.authenticate_password(user, password).await?;
	if !auth.success() {
		return Err(Error::SshAuth);
	}
	let channel = session.channel_open_session().await?;
	channel.request_shell(true).await?;
	let fingerprint = fingerprint.lock().unwrap().clone();
	Ok(Shell { stream: channel.into_stream(), session, fingerprint })
}
