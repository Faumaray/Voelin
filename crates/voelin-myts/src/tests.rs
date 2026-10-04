use super::*;
use std::time::Duration;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
};

async fn mock(
	status: u16,
	extra_headers: &str,
	body: Vec<u8>,
) -> (Client, tokio::task::JoinHandle<(String, Vec<u8>)>) {
	mock_typed(status, "application/ts3cloud", extra_headers, body).await
}

async fn mock_typed(
	status: u16,
	content_type: &str,
	extra_headers: &str,
	body: Vec<u8>,
) -> (Client, tokio::task::JoinHandle<(String, Vec<u8>)>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = Client {
		transport: transport::Transport::mock(
			format!("http://{}", listener.local_addr().unwrap()),
			Duration::from_secs(2),
		),
	};
	let extra_headers = extra_headers.to_owned();
	let content_type = content_type.to_owned();
	let task = tokio::spawn(async move {
		let (mut stream, _) = listener.accept().await.unwrap();
		let mut request = Vec::new();
		let end = loop {
			let mut byte = [0];
			stream.read_exact(&mut byte).await.unwrap();
			request.push(byte[0]);
			if request.ends_with(b"\r\n\r\n") {
				break request.len();
			}
			assert!(request.len() < 8192);
		};
		let headers = String::from_utf8(request[..end].to_vec()).unwrap();
		let length: usize = headers
			.lines()
			.find_map(|line| {
				line.to_ascii_lowercase().strip_prefix("content-length: ").map(str::to_owned)
			})
			.unwrap()
			.parse()
			.unwrap();
		let mut request_body = vec![0; length];
		stream.read_exact(&mut request_body).await.unwrap();
		let response = format!(
			"HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
			body.len()
		);
		let _ = stream.write_all(response.as_bytes()).await;
		let _ = stream.write_all(&body).await;
		(headers, request_body)
	});
	(client, task)
}
fn success() -> api::LoginSession {
	api::LoginSession {
		error: 200,
		session: "session-secret".into(),
		uuid: "account-uuid".into(),
		username: "user".into(),
		alternative_login_info: Some(api::login_session::AlternativeLoginInformation {
			renewal_token: "renewal-secret".into(),
			otp_renewal_token: "otp-secret".into(),
			device_id: "device".into(),
		}),
		..Default::default()
	}
}

#[test]
fn login_password_matches_independent_vectors_without_normalizing_passwords() {
	for email in ["User@Example.TEST", "user@example.test"] {
		assert_eq!(
			login_password(email, "password"),
			"5SG5V/18cFF2iHUFXNrFLrmLeAtbjg0sLgKpSMTZe9W8G/rPorrJyY6uJDm98ICk"
		);
	}
	assert_eq!(
		login_password("Case@Example.TEST", " Pässword "),
		"NssDyxUbagUiUJS/7Mh7YCeMmF+w3BiAO8uLCgKW0nkH0hQLWYEjuLlV5P28GHNS"
	);
	assert_eq!(
		login_password("éMAIL@Example.TEST", "páss"),
		"x6kph1Mku46TD2/zDER1UbqU0w6WV27J6W8t1CNmnDv1JELm6MPKlTyh9Mj+REjB"
	);
}

#[tokio::test]
async fn protobuf_response_accepts_the_services_json_content_type() {
	let (client, server) = mock_typed(200, "application/json", "", wire::encode(&success())).await;
	let login = client.login("mail", "password", None, "device").await.unwrap();
	assert_eq!(login.token.as_str(), "session-secret");
	server.await.unwrap();
}

#[tokio::test]
async fn login_uses_official_method_prefix_instead_of_a_header() {
	let (client, server) = mock(200, "", wire::encode(&success())).await;
	client.login("mail", "password", None, "device").await.unwrap();
	let (headers, body) = server.await.unwrap();
	assert!(body.starts_with(b"\x05login"));
	assert!(!headers.to_ascii_lowercase().contains("x-rpc-method"));
	let request: api::LoginData = wire::decode(&body[6..]).unwrap();
	assert_eq!(request.email, "mail");
	// Independent Python hashlib.pbkdf2_hmac fixture with synthetic credentials.
	assert_eq!(
		request.password,
		"TRd/AoYKGWZQ+grEk+YpdbXvhzGyZCyaA358SI+/q99VuAVHz1kByIEF5sNDSqEu"
	);
}

#[tokio::test]
async fn login_roundtrip_keeps_credentials_in_body_and_redacts_results() {
	let (client, server) = mock(200, "", wire::encode(&success())).await;
	let login = client.login("mail", "password-secret", Some("123456"), "device").await.unwrap();
	assert_eq!(login.token.as_str(), "session-secret");
	assert_eq!(login.username, "user");
	assert_eq!(login.uuid, "account-uuid");
	assert_eq!(login.identity.as_ref().unwrap_err(), &IdentityError::Missing);
	assert!(!format!("{login:?}").contains("secret"));
	let renewal = login.renewal.unwrap();
	assert_eq!(renewal.token, "renewal-secret");
	assert_eq!(renewal.otp_token, "otp-secret");
	assert_eq!(renewal.device_id, "device");
	assert!(!format!("{renewal:?}").contains("secret"));
	let (headers, body) = server.await.unwrap();
	assert!(headers.starts_with("POST /authentication HTTP/1.1"));
	assert!(headers.to_lowercase().contains("content-type: application/ts3cloud"));
	assert!(body.starts_with(b"\x05login"));
	assert!(!headers.contains("password-secret"));
	let data: api::LoginData = wire::decode(&body[6..]).unwrap();
	assert_eq!(data.email, "mail");
	assert_eq!(data.password, "MQU48au/lEdA0Hd/DNJ09/M4ZJgqYL5Q+PRF7OILYbUkZWHIpGw4c/kW4t4/Qofk");
	assert_eq!(data.otp, "123456");
	assert_eq!(data.device_id, "device");
	assert!(!data.skip_session);
}

#[tokio::test]
async fn login_requires_explicit_success_nonempty_token_and_preserves_status() {
	for code in [0, 202, 208, 98765] {
		let mut response = success();
		response.error = code;
		let (client, server) = mock(200, "", wire::encode(&response)).await;
		let error = client.login("e", "p", None, "d").await.unwrap_err();
		assert_eq!(error.status_code(), Some(code));
		assert_eq!(error.is_otp_required(), code == 208);
		server.await.unwrap();
	}
	let mut response = success();
	response.session.clear();
	let (client, server) = mock(200, "", wire::encode(&response)).await;
	assert!(matches!(
		client.login("e", "p", None, "d").await,
		Err(Error::Session(session::SessionError::EmptyToken))
	));
	server.await.unwrap();
}

#[tokio::test]
async fn remembered_otp_requires_a_device_and_yields_to_an_explicit_code() {
	for (device, otp, expected_token) in
		[("device", None, "otp-renewal"), ("", None, ""), ("device", Some("123456"), "")]
	{
		let (client, server) = mock(200, "", wire::encode(&success())).await;
		client
			.login_with_otp_renewal("mail", "password", otp, device, "otp-renewal")
			.await
			.unwrap();
		let (_, body) = server.await.unwrap();
		let request: api::LoginData = wire::decode(&body[6..]).unwrap();
		assert_eq!(request.otp_renewal_token, expected_token);
		assert_eq!(request.device_id, device);
		assert_eq!(request.otp, otp.unwrap_or_default());
		assert!(!request.password.is_empty());
		assert_ne!(request.password, "password");
	}
}

#[tokio::test]
async fn renewal_encodes_exact_tokens_and_rejects_missing_credentials_before_network() {
	{
		let auth = "auth-secret";
		let (client, server) = mock(200, "", wire::encode(&success())).await;
		let login = client.login_with_renewal("renewal-secret", auth).await.unwrap();
		assert_eq!(login.identity.unwrap_err(), IdentityError::PasswordRequired);
		let (headers, body) = server.await.unwrap();
		assert!(headers.starts_with("POST /authentication HTTP/1.1"));
		assert!(body.starts_with(b"\x15loginWithRenewalToken"));
		let request: api::RenewalTokenLogin = wire::decode(&body[22..]).unwrap();
		assert_eq!(request.renewal_token, "renewal-secret");
		assert_eq!(request.auth_token, auth);
	}
	let client = Client {
		transport: transport::Transport::mock("http://127.0.0.1:1".into(), Duration::from_secs(1)),
	};
	for (renewal, auth) in [("", "auth"), ("renewal", ""), ("", "")] {
		assert!(matches!(
			client.login_with_renewal(renewal, auth).await,
			Err(Error::MissingRenewalCredentials)
		));
	}
}

#[tokio::test]
async fn session_validation_and_deletion_have_distinct_success_statuses() {
	let token = SessionToken::new("secret").unwrap();
	for (logout, code, valid) in [
		(false, 102, true),
		(false, 103, false),
		(false, 104, false),
		(false, 204, false),
		(true, 204, true),
		(true, 102, false),
	] {
		let (client, server) =
			mock(200, "", wire::encode(&api::LoginStatus { error: code, ..Default::default() }))
				.await;
		let result = if logout {
			client.logout(&token).await
		} else {
			client.validate_session(&token).await
		};
		assert_eq!(result.is_ok(), valid);
		if let Err(error) = result {
			assert_eq!(error.is_invalid_session(), code == 103);
		}
		let (headers, body) = server.await.unwrap();
		assert!(headers.starts_with("POST /session HTTP/1.1"));
		let prefix: &[u8] = if logout { b"\x0ddeleteSession" } else { b"\x07session" };
		assert!(body.starts_with(prefix));
		assert_eq!(wire::decode::<api::Session>(&body[prefix.len()..]).unwrap().session, "secret");
	}
}

#[tokio::test]
async fn transport_rejects_http_redirects_oversize_and_malformed_data() {
	for (status, body) in
		[(302, vec![]), (500, vec![]), (200, vec![0; transport::MAX_BODY + 1]), (200, vec![0xff])]
	{
		let (client, server) = mock(status, "Location: http://127.0.0.1:1/leak\r\n", body).await;
		let error = client.login("e", "p", None, "d").await.unwrap_err();
		match status {
			302 | 500 => assert!(matches!(error, Error::Http(code) if code == status)),
			_ => assert!(matches!(error, Error::ResponseTooLarge | Error::Decode(_))),
		}
		server.await.unwrap();
	}
	let client = Client {
		transport: transport::Transport::mock("http://127.0.0.1:1".into(), Duration::from_secs(1)),
	};
	assert!(matches!(
		client.login("e", &"x".repeat(transport::MAX_BODY), None, "d").await,
		Err(Error::RequestTooLarge)
	));
	assert!(matches!(client.login("e", "p", None, "d").await, Err(Error::Transport(_))));
}

#[tokio::test]
async fn transport_bounds_chunked_responses_and_slow_servers() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = Client {
		transport: transport::Transport::mock(
			format!("http://{}", listener.local_addr().unwrap()),
			Duration::from_secs(2),
		),
	};
	let server = tokio::spawn(async move {
		let (mut stream, _) = listener.accept().await.unwrap();
		let mut request = [0; 1024];
		let _ = stream.read(&mut request).await;
		stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/ts3cloud\r\nTransfer-Encoding: chunked\r\n\r\n10001\r\n").await.unwrap();
		let _ = stream.write_all(&vec![0; transport::MAX_BODY + 1]).await;
		let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
	});
	assert!(matches!(client.login("e", "p", None, "d").await, Err(Error::ResponseTooLarge)));
	server.await.unwrap();
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = Client {
		transport: transport::Transport::mock(
			format!("http://{}", listener.local_addr().unwrap()),
			Duration::from_millis(25),
		),
	};
	assert!(matches!(client.login("e", "p", None, "d").await, Err(Error::Timeout)));
}

#[tokio::test]
async fn transport_rejects_wrong_content_type_and_encoded_overhead() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = Client {
		transport: transport::Transport::mock(
			format!("http://{}", listener.local_addr().unwrap()),
			Duration::from_secs(2),
		),
	};
	let server = tokio::spawn(async move {
		let (mut stream, _) = listener.accept().await.unwrap();
		let mut request = [0; 1024];
		let _ = stream.read(&mut request).await;
		stream
			.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n")
			.await
			.unwrap();
	});
	assert!(matches!(client.login("e", "p", None, "d").await, Err(Error::ContentType)));
	server.await.unwrap();
	// The method prefix counts towards the request bound too.
	assert!(matches!(
		client.transport.call("authentication", "login", &vec![0; transport::MAX_BODY - 5]).await,
		Err(Error::RequestTooLarge)
	));
}

#[tokio::test]
#[ignore = "owner-authorized live account check; explicit credentials and opt-in required"]
async fn owner_live_login_validate_logout() {
	assert_eq!(
		std::env::var("VOELIN_MYTS_LIVE").as_deref(),
		Ok("1"),
		"explicit live opt-in required"
	);
	let email = std::env::var("VOELIN_MYTS_EMAIL").expect("email required");
	let password = std::env::var("VOELIN_MYTS_PASSWORD").expect("password required");
	let device = std::env::var("VOELIN_MYTS_DEVICE_ID").expect("device ID required");
	let otp = std::env::var("VOELIN_MYTS_OTP").ok();
	let client = Client::new().unwrap();
	let login =
		client.login(&email, &password, otp.as_deref(), &device).await.expect("live login failed");
	let validation = client.validate_session(&login.token).await;
	let logout = client.logout(&login.token).await;
	validation.expect("live validation failed");
	logout.expect("live logout failed");
}

#[tokio::test]
async fn unsupported_identity_encryption_preserves_successful_account_login() {
	let mut response = success();
	response.key = vec![1];
	response.myts_id_data = Some(api::MyTeamSpeakIdData {
		user_public_key: vec![0; 32],
		user_private_key: Some(Default::default()),
		..Default::default()
	});
	let (client, server) = mock(200, "", wire::encode(&response)).await;
	let login = client.login("mail", "password", None, "device").await.unwrap();
	assert_eq!(login.token.as_str(), "session-secret");
	assert_eq!(login.identity.unwrap_err(), IdentityError::UnsupportedKeyVersion);
	server.await.unwrap();
}
