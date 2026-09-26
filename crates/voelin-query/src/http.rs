//! HTTP WebQuery: `GET /<sid>/<command>?key=value` with an `x-api-key` header,
//! answered with JSON `{"body": [...], "status": {"code": 0, "message": "ok"}}`.

use serde::Deserialize;

use crate::codec::{Command, QueryError, Row};
use crate::{Error, Result};

#[derive(Clone)]
pub struct HttpClient {
	http: reqwest::Client,
	base: String,
	api_key: Option<String>,
	sid: u32,
}

#[derive(Deserialize)]
struct Status {
	code: u32,
	message: String,
	#[serde(default)]
	extra_message: Option<String>,
	#[serde(default)]
	failed_permission: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Reply {
	#[serde(default)]
	body: Vec<serde_json::Map<String, serde_json::Value>>,
	status: Status,
}

impl HttpClient {
	/// `base` is e.g. `http://host:10080`. Without an API key requests run
	/// with the server's guest query permissions.
	pub fn new(base: &str, api_key: Option<String>, sid: u32) -> Result<Self> {
		let http = reqwest::Client::builder().build()?;
		Ok(Self { http, base: base.trim_end_matches('/').to_string(), api_key, sid })
	}

	pub fn set_sid(&mut self, sid: u32) {
		self.sid = sid;
	}

	pub async fn send(&self, cmd: &Command) -> Result<Vec<Row>> {
		// Multi-row commands are not expressible as query parameters.
		if cmd.rows.len() > 1 {
			return Err(Error::Unsupported("multi-row commands over HTTP"));
		}
		// Instance-level commands have no server id in the path.
		let url = if matches!(cmd.name.as_str(), "serverlist" | "version" | "hostinfo") {
			format!("{}/{}", self.base, cmd.name)
		} else {
			format!("{}/{}/{}", self.base, self.sid, cmd.name)
		};
		let mut params: Vec<(String, String)> = cmd.rows[0].clone();
		params.extend(cmd.flags.iter().map(|f| (f.clone(), String::new())));
		let mut req = self.http.get(url).query(&params);
		if let Some(key) = &self.api_key {
			req = req.header("x-api-key", key);
		}
		let reply: Reply = req.send().await?.json().await?;
		if reply.status.code != 0 && reply.status.code != 1281 {
			return Err(QueryError {
				id: reply.status.code,
				msg: reply.status.message,
				extra_msg: reply.status.extra_message.or_else(|| {
					reply.status.failed_permission.map(|p| format!("missing permission {p}"))
				}),
				failed_permid: None,
			}
			.into());
		}
		Ok(reply
			.body
			.into_iter()
			.map(|obj| {
				Row(obj
					.into_iter()
					.map(|(k, v)| {
						let v = match v {
							serde_json::Value::String(s) => s,
							serde_json::Value::Null => String::new(),
							other => other.to_string(),
						};
						(k, v)
					})
					.collect())
			})
			.collect())
	}
}
