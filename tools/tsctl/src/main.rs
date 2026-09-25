//! `tsctl`: headless TeamSpeak 3/6 client for development and integration tests.

mod gateway;
mod identity;
mod observe;
mod query;
mod session;
mod tree;
mod versions;
mod voice;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// Manage the client identity (the key pair that gives the client its unique id).
	Identity {
		#[command(subcommand)]
		command: IdentityCommand,
	},
	/// List the signed client versions that can be sent in `clientinit`.
	Versions {
		/// Only show versions whose platform or version contains this text.
		filter: Option<String>,
	},
	/// Connect to a server and run an action.
	Connect(ConnectArgs),
	/// Run ServerQuery commands (raw TCP, SSH or HTTP WebQuery).
	Query(query::QueryArgs),
	/// Watch who is in which channel through ServerQuery, without appearing on the server.
	Observe(observe::ObserveArgs),
	/// Read and write a channel's chat through an invisible query relay.
	Relay(observe::RelayArgs),
	/// Use a tsgw gateway as a user: presence, chat, history.
	Gateway(gateway::GatewayArgs),
}

#[derive(Subcommand, Debug)]
enum IdentityCommand {
	/// Create a new identity and store it.
	New {
		/// Minimum security level (hash cash). Servers commonly require 8.
		#[arg(long, default_value_t = 8)]
		level: u8,
		/// Where to store the identity [default: <config dir>/tsctl/identity.json].
		#[arg(long)]
		out: Option<PathBuf>,
		/// Overwrite an existing file.
		#[arg(long)]
		force: bool,
	},
	/// Print uid and security level of a stored identity.
	Show {
		#[arg(long)]
		identity: Option<PathBuf>,
	},
}

#[derive(Args, Debug)]
pub struct ConnectArgs {
	/// Server address: host, host:port, or a TeamSpeak server nickname.
	address: String,
	/// Nickname to use.
	#[arg(long, default_value = "tsctl")]
	nick: String,
	/// Identity file. Without it a temporary identity is generated.
	#[arg(long, env = "TSCTL_IDENTITY")]
	identity: Option<PathBuf>,
	/// Signed client version: `default`, an index or `<platform>@<version>` from `tsctl versions`.
	#[arg(long, default_value = "default")]
	client_version: String,
	/// Server password.
	#[arg(long, env = "TSCTL_SERVER_PASSWORD")]
	server_password: Option<String>,
	/// Privilege key (token) to redeem after connecting, e.g. the ServerAdmin key a new server prints.
	#[arg(long, env = "TSCTL_PRIVILEGE_KEY")]
	privilege_key: Option<String>,
	/// Channel to join, as a path like `Lobby/Sub`.
	#[arg(long)]
	channel: Option<String>,
	/// Log every command sent and received.
	#[arg(long)]
	log_commands: bool,
	/// Give up connecting after this many seconds.
	#[arg(long, default_value_t = 30)]
	connect_timeout: u64,
	#[command(subcommand)]
	action: Action,
}

#[derive(Subcommand, Debug)]
pub enum Action {
	/// Subscribe to all channels and print the channel tree with its clients.
	Tree {
		/// How long to wait for subscription results before printing.
		#[arg(long, default_value_t = 1500)]
		settle_ms: u64,
	},
	/// Send a text message and exit.
	Chat {
		#[command(subcommand)]
		target: ChatTarget,
	},
	/// Print text messages and client events until interrupted.
	Listen {
		/// Print events as JSON lines instead of text.
		#[arg(long)]
		json: bool,
		/// Exit successfully once a text message containing this text arrives.
		#[arg(long)]
		expect: Option<String>,
		/// Exit after this many seconds; fails if `--expect` was not satisfied.
		#[arg(long)]
		timeout: Option<u64>,
	},
	/// Send a raw, already escaped command (e.g. `serveredit virtualserver_name=Test\\sServer`)
	/// and wait for the result. Combine with `--log-commands` to see the answer.
	Raw { command: String },
	/// Send or record voice in the current channel.
	Voice {
		#[command(subcommand)]
		command: VoiceCommand,
	},
	/// Interactive session. Type `/help` for commands.
	Repl,
}

#[derive(Subcommand, Debug, Clone)]
pub enum VoiceCommand {
	/// Talk: send a WAV file or a test tone, in real time.
	Send {
		/// WAV file to send (any sample rate; resampled to 48 kHz).
		file: Option<PathBuf>,
		/// Send a sine tone of this frequency instead of a file.
		#[arg(long, conflicts_with = "file")]
		tone: Option<f32>,
		/// Tone length in seconds.
		#[arg(long, default_value_t = 3.0)]
		seconds: f32,
		/// Use the stereo Opus Music codec instead of mono Opus Voice.
		#[arg(long)]
		music: bool,
	},
	/// Listen: mix everything said in the channel into a stereo 48 kHz WAV file.
	Record {
		out: PathBuf,
		/// Recording length in seconds.
		#[arg(long, default_value_t = 5.0)]
		seconds: f32,
		/// Fail unless the recording contains this tone frequency (for tests).
		#[arg(long)]
		expect_tone: Option<f32>,
	},
}

#[derive(Subcommand, Debug, Clone)]
pub enum ChatTarget {
	/// Send to the server chat (everyone on the server).
	Server { message: String },
	/// Send to the chat of the channel we are in.
	Channel { message: String },
	/// Send a private message to a client id.
	Client { clid: u16, message: String },
}

fn main() -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
		)
		.with_writer(std::io::stderr)
		.init();

	let cli = Cli::parse();
	match cli.command {
		Command::Identity { command } => run_identity(command),
		Command::Versions { filter } => {
			for (i, v) in versions::known_versions().iter().enumerate() {
				let matches = filter
					.as_deref()
					.is_none_or(|f| v.platform.contains(f) || v.version.contains(f));
				if matches {
					println!("{i:>3}  {}@{}", v.platform, v.version);
				}
			}
			Ok(())
		}
		Command::Connect(args) => {
			let rt = tokio::runtime::Runtime::new()?;
			rt.block_on(session::run(args))
		}
		Command::Query(args) => tokio::runtime::Runtime::new()?.block_on(query::run(args)),
		Command::Observe(args) => tokio::runtime::Runtime::new()?.block_on(observe::observe(args)),
		Command::Relay(args) => tokio::runtime::Runtime::new()?.block_on(observe::relay(args)),
		Command::Gateway(args) => tokio::runtime::Runtime::new()?.block_on(gateway::run(args)),
	}
}

fn run_identity(command: IdentityCommand) -> Result<()> {
	match command {
		IdentityCommand::New { level, out, force } => {
			let path = out.unwrap_or_else(identity::default_path);
			if path.exists() && !force {
				anyhow::bail!("{} already exists, pass --force to overwrite", path.display());
			}
			let id = identity::create(level);
			identity::save(&path, &id)?;
			println!("uid:   {}", identity::uid(&id));
			println!("level: {}", id.level());
			println!("saved: {}", path.display());
			Ok(())
		}
		IdentityCommand::Show { identity: path } => {
			let path = path.unwrap_or_else(identity::default_path);
			let id = identity::load(&path)?;
			println!("uid:   {}", identity::uid(&id));
			println!("level: {}", id.level());
			println!("omega: {}", id.key().to_pub().to_ts());
			Ok(())
		}
	}
}

impl ConnectArgs {
	fn connect_timeout(&self) -> Duration {
		Duration::from_secs(self.connect_timeout)
	}
}
