//! Mirror the server's permission checks for users who are not connected.
//!
//! `permoverview cid=<channel> cldbid=<user> permid=0` lists every permission
//! assignment relevant to a user in a channel, one row per level:
//! `t=0` server group, `t=1` client, `t=2` channel, `t=3` channel group,
//! `t=4` channel client; `v` value, `n` negate, `s` skip.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tsc_query::{Command, QueryClient, Row};

const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
	pub level: u8,
	pub perm: u32,
	pub value: i64,
	pub negate: bool,
	pub skip: bool,
}

impl Entry {
	fn from_row(row: &Row) -> Option<Self> {
		Some(Self {
			level: row.parse("t")?,
			perm: row.parse("p")?,
			value: row.parse("v")?,
			negate: row.flag("n").unwrap_or(false),
			skip: row.flag("s").unwrap_or(false),
		})
	}
}

/// The user's effective value of `perm` (0 if not granted anywhere).
///
/// Server groups combine to the highest value (lowest among negated ones);
/// a client permission replaces them; channel group and channel-client
/// permissions replace those unless a skip flag was set at the server group
/// or client level.
pub fn effective(entries: &[Entry], perm: u32) -> i64 {
	let of = |level: u8| entries.iter().filter(move |e| e.perm == perm && e.level == level);
	let groups: Vec<_> = of(0).collect();
	let mut value = if groups.iter().any(|e| e.negate) {
		groups.iter().filter(|e| e.negate).map(|e| e.value).min().unwrap_or(0)
	} else {
		groups.iter().map(|e| e.value).max().unwrap_or(0)
	};
	let mut skip = groups.iter().any(|e| e.skip);
	if let Some(client) = of(1).next() {
		value = client.value;
		skip |= client.skip;
	}
	if !skip {
		if let Some(group) = of(3).next() {
			value = group.value;
		}
		if let Some(channel_client) = of(4).next() {
			value = channel_client.value;
		}
	}
	value
}

/// A permission the channel itself sets (level 2), e.g. `i_channel_needed_join_power`.
pub fn channel_value(entries: &[Entry], perm: u32) -> i64 {
	entries.iter().find(|e| e.perm == perm && e.level == 2).map(|e| e.value).unwrap_or(0)
}

/// Permission ids by name; they differ between server versions.
#[derive(Clone, Copy, Debug, Default)]
pub struct PermIds {
	pub join_power: u32,
	pub needed_join_power: u32,
	pub subscribe_power: u32,
	pub channel_text_send: u32,
	pub server_text_send: u32,
	pub ignore_password: u32,
}

impl PermIds {
	pub async fn load(client: &QueryClient) -> tsc_query::Result<Self> {
		let names = [
			"i_channel_join_power",
			"i_channel_needed_join_power",
			"i_channel_subscribe_power",
			"b_client_channel_textmessage_send",
			"b_client_server_textmessage_send",
			"b_channel_join_ignore_password",
		];
		let mut ids = [0u32; 6];
		for (i, name) in names.iter().enumerate() {
			let rows = client.send(&Command::new("permidgetbyname").arg("permsid", name)).await?;
			ids[i] = rows
				.first()
				.and_then(|r| r.parse("permid"))
				.ok_or_else(|| tsc_query::Error::Protocol(format!("unknown permission {name}")))?;
		}
		Ok(Self {
			join_power: ids[0],
			needed_join_power: ids[1],
			subscribe_power: ids[2],
			channel_text_send: ids[3],
			server_text_send: ids[4],
			ignore_password: ids[5],
		})
	}
}

/// What a user may do in a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelAccess {
	/// May join, so may see the channel chat.
	pub read: bool,
	/// May also write in the channel chat.
	pub post: bool,
}

/// Decide channel access from `permoverview` entries.
pub fn channel_access(entries: &[Entry], ids: &PermIds, has_password: bool) -> ChannelAccess {
	let can_join = effective(entries, ids.join_power)
		>= channel_value(entries, ids.needed_join_power)
		&& (!has_password || effective(entries, ids.ignore_password) > 0);
	ChannelAccess {
		read: can_join,
		post: can_join && effective(entries, ids.channel_text_send) > 0,
	}
}

/// `(cldbid, cid)` to the time of the lookup and its entries.
type Cache = HashMap<(u64, u64), (Instant, Vec<Entry>)>;

/// Caches `permoverview` per (user, channel).
pub struct PermResolver {
	pub ids: PermIds,
	cache: Mutex<Cache>,
}

impl PermResolver {
	pub fn new(ids: PermIds) -> Self {
		Self { ids, cache: Default::default() }
	}

	pub async fn entries(
		&self,
		client: &QueryClient,
		cldbid: u64,
		cid: u64,
	) -> tsc_query::Result<Vec<Entry>> {
		if let Some((at, entries)) = self.cache.lock().unwrap().get(&(cldbid, cid))
			&& at.elapsed() < CACHE_TTL
		{
			return Ok(entries.clone());
		}
		let rows = client
			.send(
				&Command::new("permoverview")
					.arg("cid", cid)
					.arg("cldbid", cldbid)
					.arg("permid", 0),
			)
			.await?;
		let entries: Vec<Entry> = rows.iter().filter_map(Entry::from_row).collect();
		self.cache.lock().unwrap().insert((cldbid, cid), (Instant::now(), entries.clone()));
		Ok(entries)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn e(level: u8, perm: u32, value: i64) -> Entry {
		Entry { level, perm, value, negate: false, skip: false }
	}

	#[test]
	fn server_groups_take_the_highest() {
		assert_eq!(effective(&[e(0, 1, 10), e(0, 1, 50), e(0, 2, 99)], 1), 50);
		assert_eq!(effective(&[], 1), 0);
	}

	#[test]
	fn negate_takes_lowest_negated() {
		let mut neg = e(0, 1, 5);
		neg.negate = true;
		assert_eq!(effective(&[e(0, 1, 75), neg], 1), 5);
	}

	#[test]
	fn levels_override_in_order() {
		// client > server groups; channel group and channel client on top.
		assert_eq!(effective(&[e(0, 1, 10), e(1, 1, 20)], 1), 20);
		assert_eq!(effective(&[e(0, 1, 10), e(1, 1, 20), e(3, 1, 30)], 1), 30);
		assert_eq!(effective(&[e(0, 1, 10), e(3, 1, 30), e(4, 1, 40)], 1), 40);
	}

	#[test]
	fn skip_protects_group_value() {
		let mut skip = e(0, 1, 100);
		skip.skip = true;
		assert_eq!(effective(&[skip, e(3, 1, 0), e(4, 1, 0)], 1), 100);
	}

	#[test]
	fn channel_access_rules() {
		let ids = PermIds {
			join_power: 1,
			needed_join_power: 2,
			subscribe_power: 3,
			channel_text_send: 4,
			server_text_send: 5,
			ignore_password: 6,
		};
		// Guest in an open channel: can read and post.
		let guest = [e(0, 4, 1)];
		assert_eq!(channel_access(&guest, &ids, false), ChannelAccess { read: true, post: true });
		// Channel needs join power 50.
		let locked = [e(0, 4, 1), e(2, 2, 50)];
		assert_eq!(
			channel_access(&locked, &ids, false),
			ChannelAccess { read: false, post: false }
		);
		let strong = [e(0, 4, 1), e(2, 2, 50), e(0, 1, 75)];
		assert!(channel_access(&strong, &ids, false).read);
		// Password protected: only with the ignore-password permission.
		assert!(!channel_access(&guest, &ids, true).read);
		assert!(channel_access(&[e(0, 4, 1), e(0, 6, 1)], &ids, true).read);
		// No text permission: read only.
		assert_eq!(channel_access(&[], &ids, false), ChannelAccess { read: true, post: false });
	}
}
