//! Percent escapes in links: file links ([`crate::FileRef`]) and server
//! links ([`crate::Link`]).

/// `%XX` escapes to bytes, read as UTF-8 (a `+` stays: file names and
/// passwords keep theirs).
pub(crate) fn decode(s: &str) -> String {
	let bytes = s.as_bytes();
	let hex = |b: u8| (b as char).to_digit(16);
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%'
			&& let (Some(h), Some(l)) =
				(bytes.get(i + 1).and_then(|b| hex(*b)), bytes.get(i + 2).and_then(|b| hex(*b)))
		{
			out.push((h * 16 + l) as u8);
			i += 3;
			continue;
		}
		out.push(bytes[i]);
		i += 1;
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// Every byte but letters, digits and `-._~` as `%XX`.
pub(crate) fn encode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
			out.push(b as char);
		} else {
			out.push_str(&format!("%{b:02X}"));
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn escapes() {
		assert_eq!(decode("Raid%20Night"), "Raid Night");
		assert_eq!(decode("%C3%84b"), "Äb");
		assert_eq!(decode("a+b"), "a+b");
		// A broken escape stays as it is.
		assert_eq!(decode("100%"), "100%");
		assert_eq!(decode("%zz"), "%zz");
		assert_eq!(encode("Chill & Co/Ä"), "Chill%20%26%20Co%2F%C3%84");
		assert_eq!(decode(&encode("a b+c%")), "a b+c%");
	}
}
