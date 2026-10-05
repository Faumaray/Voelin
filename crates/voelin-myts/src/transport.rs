use crate::Error;
use std::time::Duration;

pub(crate) const MAX_BODY: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
const CONTENT_TYPE: &str = "application/ts3cloud";

pub(crate) fn check_input_size(fields: &[&str]) -> Result<(), Error> {
	if fields
		.iter()
		.try_fold(0usize, |sum, field| sum.checked_add(field.len()))
		.is_none_or(|size| size > MAX_BODY)
	{
		return Err(Error::RequestTooLarge);
	}
	Ok(())
}

#[derive(Clone)]
pub(crate) struct Transport {
	client: reqwest::Client,
	base: String,
	timeout: Duration,
}
impl Transport {
	pub(crate) fn new() -> Result<Self, Error> {
		Ok(Self {
			client: reqwest::Client::builder()
				.redirect(reqwest::redirect::Policy::none())
				.connect_timeout(Duration::from_secs(10))
				.timeout(TIMEOUT)
				.user_agent("Voelin/0.1")
				.build()?,
			base: "https://clientapi.myteamspeak.com".into(),
			timeout: TIMEOUT,
		})
	}

	#[cfg(test)]
	pub(crate) fn mock(base: String, timeout: Duration) -> Self {
		Self {
			client: reqwest::Client::builder()
				.no_proxy()
				.redirect(reqwest::redirect::Policy::none())
				.build()
				.unwrap(),
			base,
			timeout,
		}
	}

	/// GET a file from a link the service handed out (an avatar's), at most
	/// `max` bytes.
	pub(crate) async fn download(&self, url: &str, max: usize) -> Result<Vec<u8>, Error> {
		tokio::time::timeout(self.timeout, async {
			let mut response = self.client.get(url).send().await?;
			if !response.status().is_success() {
				return Err(Error::Download(response.status().as_u16()));
			}
			if response.content_length().is_some_and(|length| length > max as u64) {
				return Err(Error::ResponseTooLarge);
			}
			let mut bytes = Vec::new();
			while let Some(chunk) = response.chunk().await? {
				if chunk.len() > max - bytes.len() {
					return Err(Error::ResponseTooLarge);
				}
				bytes.extend_from_slice(&chunk);
			}
			Ok(bytes)
		})
		.await
		.map_err(|_| Error::Timeout)?
	}

	pub(crate) async fn call(
		&self,
		service: &str,
		method: &str,
		body: &[u8],
	) -> Result<Vec<u8>, Error> {
		// teamprotobufrpc prefixes the protobuf with a single-byte method-name
		// length and the method bytes. Both official desktop clients use this
		// envelope; HTTP responses contain only the protobuf message.
		if method.len() > 127 || body.len() > MAX_BODY - method.len() - 1 {
			return Err(Error::RequestTooLarge);
		}
		let mut request_body = Vec::with_capacity(1 + method.len() + body.len());
		request_body.push(method.len() as u8);
		request_body.extend_from_slice(method.as_bytes());
		request_body.extend_from_slice(body);
		tokio::time::timeout(self.timeout, async {
			let mut response = self
				.client
				.post(format!("{}/{service}", self.base))
				.header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE)
				.body(request_body)
				.send()
				.await?;
			if !response.status().is_success() {
				return Err(Error::Http(response.status().as_u16()));
			}
			if response.content_length().is_some_and(|length| length > MAX_BODY as u64) {
				return Err(Error::ResponseTooLarge);
			}
			// The live service labels binary protobuf replies application/json.
			// Accept that observed label; callers still decode and validate protobuf.
			if response
				.headers()
				.get(reqwest::header::CONTENT_TYPE)
				.and_then(|value| value.to_str().ok())
				.and_then(|value| value.split(';').next())
				.is_none_or(|value| {
					!value.trim().eq_ignore_ascii_case(CONTENT_TYPE)
						&& !value.trim().eq_ignore_ascii_case("application/json")
				}) {
				return Err(Error::ContentType);
			}
			let mut bytes = Vec::new();
			while let Some(chunk) = response.chunk().await? {
				if chunk.len() > MAX_BODY - bytes.len() {
					return Err(Error::ResponseTooLarge);
				}
				bytes.extend_from_slice(&chunk);
			}
			Ok(bytes)
		})
		.await
		.map_err(|_| Error::Timeout)?
	}
}
