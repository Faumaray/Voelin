//! `tsctl query`: ServerQuery commands and events.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use tokio::io::{AsyncBufReadExt, BufReader};
use tsc_query::{Command, Connect, LineOptions, QueryClient, Row, Transport};

/// How to reach and log into a ServerQuery interface.
#[derive(Args, Debug, Clone)]
pub struct QueryConnArgs {
	/// Transport: raw (TS3 only), ssh or http.
	#[arg(value_enum)]
	pub transport: TransportArg,
	/// host:port of the query interface (e.g. 127.0.0.1:10022).
	pub addr: String,
	/// Login name (raw/SSH).
	#[arg(long, default_value = "serveradmin")]
	pub user: String,
	/// Password (raw/SSH) or API key (HTTP). Without it, HTTP runs as guest.
	#[arg(long, env = "TSCTL_QUERY_SECRET", hide_env_values = true)]
	pub secret: Option<String>,
	/// Select the virtual server by its voice port instead of id 1.
	#[arg(long)]
	pub server_port: Option<u16>,
	/// Our IP is on the server's query allowlist: skip client-side rate limiting.
	#[arg(long)]
	pub allowlisted: bool,
}

#[derive(Args, Debug, Clone)]
pub struct QueryArgs {
	#[command(flatten)]
	pub conn: QueryConnArgs,
	/// After the commands, print events for this many seconds.
	#[arg(long)]
	pub events: Option<u64>,
	/// Command line to run, e.g. `clientlist -uid`. Without one, commands are read from stdin.
	#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
	pub command: Vec<String>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum TransportArg {
	Raw,
	Ssh,
	Http,
}

impl QueryConnArgs {
	pub fn connect_config(&self) -> Connect {
		Connect {
			transport: match self.transport {
				TransportArg::Raw => Transport::Raw,
				TransportArg::Ssh => Transport::Ssh,
				TransportArg::Http => Transport::Http,
			},
			addr: self.addr.clone(),
			user: self.user.clone(),
			secret: self.secret.clone(),
			server_port: self.server_port,
			server_id: None,
			line: LineOptions {
				rate_limit: if self.allowlisted { None } else { LineOptions::default().rate_limit },
				..Default::default()
			},
		}
	}
}

pub fn print_rows(rows: &[Row]) {
	for row in rows {
		let fields: Vec<String> = row.0.iter().map(|(k, v)| format!("{k}={v}")).collect();
		println!("{}", fields.join("  "));
	}
}

pub async fn run(args: QueryArgs) -> Result<()> {
	let (client, events) = QueryClient::connect(&args.conn.connect_config())
		.await
		.context("query connection failed")?;

	if args.command.is_empty() {
		let mut lines = BufReader::new(tokio::io::stdin()).lines();
		while let Some(line) = lines.next_line().await? {
			if line.trim().is_empty() {
				continue;
			}
			run_line(&client, &line).await;
		}
	} else {
		let line = args.command.join(" ");
		let Some(cmd) = Command::parse(&line) else { bail!("empty command") };
		print_rows(&client.send(&cmd).await?);
	}

	if let Some(secs) = args.events {
		let Some(mut events) = events else { bail!("this transport has no events") };
		let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
		while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
			print!("{}: ", event.name);
			print_rows(&event.rows);
		}
	}
	Ok(())
}

async fn run_line(client: &QueryClient, line: &str) {
	let Some(cmd) = Command::parse(line) else { return };
	match client.send(&cmd).await {
		Ok(rows) => {
			print_rows(&rows);
			println!("ok");
		}
		Err(error) => println!("error: {error}"),
	}
}
