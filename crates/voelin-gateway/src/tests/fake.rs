//! A fake TeamSpeak 3 ServerQuery server (raw TCP) with just enough state
//! for the gateway: channels, online clients, the client database, groups,
//! permissions, and a log of what was posted.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use voelin_query::{Command, escape};

/// Permission ids the fake server uses.
pub const JOIN_POWER: u32 = 1;
pub const SUBSCRIBE_POWER: u32 = 3;
pub const CHANNEL_TEXT: u32 = 4;
pub const SERVER_TEXT: u32 = 5;
pub const VIRTUALSERVER_MODIFY_NAME: u32 = 8;

const PERMS: &[(&str, u32)] = &[
	("i_channel_join_power", JOIN_POWER),
	("i_channel_needed_join_power", 2),
	("i_channel_subscribe_power", SUBSCRIBE_POWER),
	("b_client_channel_textmessage_send", CHANNEL_TEXT),
	("b_client_server_textmessage_send", SERVER_TEXT),
	("b_channel_join_ignore_password", 6),
	("b_channel_modify_name", 7),
	("b_virtualserver_modify_name", VIRTUALSERVER_MODIFY_NAME),
];

#[derive(Clone, Debug)]
pub struct Online {
	pub clid: u16,
	pub cid: u64,
	pub uid: String,
	pub nickname: String,
	pub server_groups: Vec<u64>,
	pub streaming: bool,
}

#[derive(Default)]
pub struct State {
	pub online: Vec<Online>,
	/// uid to (cldbid, nickname).
	pub known: HashMap<String, (u64, String)>,
	/// Extra permissions per cldbid: (perm, value) at server group level.
	pub perms: HashMap<u64, Vec<(u32, i64)>>,
	pub server_groups: HashMap<u64, Vec<u64>>,
	/// (target mode, channel of the posting connection, text)
	pub posted: Vec<(u8, u64, String)>,
	/// Nicknames set with clientupdate.
	pub nicknames: Vec<String>,
	/// Connections that registered for server events (the observer).
	observers: Vec<mpsc::UnboundedSender<String>>,
	/// Relay connections by channel.
	relays: Vec<(u64, mpsc::UnboundedSender<String>)>,
	next_clid: u16,
}

#[derive(Clone)]
pub struct FakeServer {
	pub addr: SocketAddr,
	pub state: Arc<Mutex<State>>,
}

impl FakeServer {
	pub async fn start() -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let state = Arc::new(Mutex::new(State { next_clid: 100, ..Default::default() }));
		let server = Self { addr, state };
		let s = server.clone();
		tokio::spawn(async move {
			while let Ok((stream, _)) = listener.accept().await {
				tokio::spawn(s.clone().connection(stream));
			}
		});
		server
	}

	/// A user known to the server's database (connected with voice once).
	pub fn add_known(&self, uid: &str, cldbid: u64, nickname: &str, groups: &[u64]) {
		let mut s = self.state.lock().unwrap();
		s.known.insert(uid.to_string(), (cldbid, nickname.to_string()));
		s.server_groups.insert(cldbid, groups.to_vec());
	}

	pub fn grant(&self, cldbid: u64, perm: u32, value: i64) {
		self.state.lock().unwrap().perms.entry(cldbid).or_default().push((perm, value));
	}

	pub fn add_online(&self, client: Online) {
		self.state.lock().unwrap().online.push(client);
	}

	/// Send an event to the observer's connections.
	pub fn notify_observers(&self, line: &str) {
		for tx in &self.state.lock().unwrap().observers {
			let _ = tx.send(line.to_string());
		}
	}

	/// Someone writes in a channel (seen by its relay).
	pub fn say_in_channel(&self, cid: u64, name: &str, uid: &str, text: &str) {
		let line = format!(
			"notifytextmessage targetmode=2 msg={} invokerid=77 invokername={} invokeruid={}",
			escape(text),
			escape(name),
			escape(uid)
		);
		for (c, tx) in &self.state.lock().unwrap().relays {
			if *c == cid {
				let _ = tx.send(line.clone());
			}
		}
	}

	pub fn posted(&self) -> Vec<(u8, u64, String)> {
		self.state.lock().unwrap().posted.clone()
	}

	async fn connection(self, stream: tokio::net::TcpStream) {
		let (r, mut w) = stream.into_split();
		let mut lines = BufReader::new(r).lines();
		let (tx, mut rx) = mpsc::unbounded_channel::<String>();
		let clid = {
			let mut s = self.state.lock().unwrap();
			s.next_clid += 1;
			s.next_clid
		};
		let mut channel = 1u64;
		if w.write_all(b"TS3\n\rWelcome to the fake ServerQuery interface.\n\r").await.is_err() {
			return;
		}
		loop {
			tokio::select! {
				line = lines.next_line() => {
					let Ok(Some(line)) = line else { return };
					let Some(cmd) = Command::parse(&line) else { continue };
					let reply = self.answer(&cmd, clid, &mut channel, &tx);
					if w.write_all(reply.as_bytes()).await.is_err() {
						return;
					}
				}
				Some(event) = rx.recv() => {
					if w.write_all(format!("{event}\n\r").as_bytes()).await.is_err() {
						return;
					}
				}
			}
		}
	}

	fn answer(
		&self,
		cmd: &Command,
		clid: u16,
		channel: &mut u64,
		tx: &mpsc::UnboundedSender<String>,
	) -> String {
		let arg = |key: &str| {
			cmd.rows.first().and_then(|r| r.iter().find(|(k, _)| k == key)).map(|(_, v)| v.clone())
		};
		let num = |key: &str| arg(key).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
		let ok = "error id=0 msg=ok\n\r".to_string();
		let empty = "error id=1281 msg=database\\sempty\\sresult\\sset\n\r".to_string();
		let data = |rows: Vec<String>| {
			if rows.is_empty() { empty.clone() } else { format!("{}\n\r{ok}", rows.join("|")) }
		};
		let mut s = self.state.lock().unwrap();
		match cmd.name.as_str() {
			"serverinfo" => data(vec![
				"virtualserver_name=Fake virtualserver_unique_identifier=fake-server-uid \
				 virtualserver_version=3.13.7\\s[Build:\\s1] \
				 virtualserver_needed_identity_security_level=0"
					.into(),
			]),
			"channellist" => data(vec![
				"cid=1 pid=0 channel_order=0 channel_name=Lobby channel_flag_default=1".into(),
				"cid=2 pid=0 channel_order=1 channel_name=Games".into(),
				"cid=3 pid=0 channel_order=2 channel_name=Staff channel_needed_subscribe_power=100"
					.into(),
			]),
			"permidgetbyname" => {
				let name = arg("permsid").unwrap_or_default();
				match PERMS.iter().find(|(n, _)| *n == name) {
					Some((_, id)) => data(vec![format!("permsid={name} permid={id}")]),
					None => "error id=2 msg=invalid\\spermission\n\r".into(),
				}
			}
			"whoami" => data(vec![format!("client_id={clid} client_channel_id={channel}")]),
			"clientupdate" => {
				if let Some(nick) = arg("client_nickname") {
					s.nicknames.push(nick);
				}
				ok
			}
			"servernotifyregister" => {
				match arg("event").as_deref() {
					Some("server") => s.observers.push(tx.clone()),
					Some("textchannel") => s.relays.push((*channel, tx.clone())),
					_ => {}
				}
				ok
			}
			"clientmove" => {
				*channel = num("cid");
				ok
			}
			"clientlist" => data(
				s.online
					.iter()
					.map(|c| {
						format!(
							"clid={} cid={} client_database_id=0 client_nickname={} client_type=0 \
							 client_unique_identifier={} client_servergroups={} client_is_streaming={}",
							c.clid,
							c.cid,
							escape(&c.nickname),
							escape(&c.uid),
							c.server_groups
								.iter()
								.map(u64::to_string)
								.collect::<Vec<_>>()
								.join(","),
							u8::from(c.streaming)
						)
					})
					.collect(),
			),
			"banlist" => empty,
			"clientdbfind" => {
				let uid = arg("pattern").unwrap_or_default();
				data(s.known.get(&uid).map(|(id, _)| format!("cldbid={id}")).into_iter().collect())
			}
			"clientdbinfo" => {
				let id = num("cldbid");
				let nick = s.known.values().find(|(c, _)| *c == id).map(|(_, n)| n.clone());
				data(nick.map(|n| format!("client_nickname={}", escape(&n))).into_iter().collect())
			}
			"servergroupsbyclientid" => data(
				s.server_groups
					.get(&num("cldbid"))
					.map(|g| g.iter().map(|g| format!("sgid={g}")).collect())
					.unwrap_or_default(),
			),
			"channelgroupclientlist" => empty,
			"permoverview" => {
				let cldbid = num("cldbid");
				let mut rows = vec![
					format!("t=0 id1=0 id2=0 p={JOIN_POWER} v=50 n=0 s=0"),
					format!("t=0 id1=0 id2=0 p={SUBSCRIBE_POWER} v=50 n=0 s=0"),
					format!("t=0 id1=0 id2=0 p={CHANNEL_TEXT} v=1 n=0 s=0"),
					format!("t=0 id1=0 id2=0 p={SERVER_TEXT} v=1 n=0 s=0"),
				];
				for (perm, value) in s.perms.get(&cldbid).into_iter().flatten() {
					rows.push(format!("t=0 id1=0 id2=0 p={perm} v={value} n=0 s=0"));
				}
				data(rows)
			}
			"sendtextmessage" => {
				let mode = num("targetmode") as u8;
				s.posted.push((mode, *channel, arg("msg").unwrap_or_default()));
				ok
			}
			_ => ok,
		}
	}
}
