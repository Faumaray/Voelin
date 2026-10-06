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

/// What servers are shown of the account comes with the login: the
/// certificate, and the avatar as it came.
#[tokio::test]
async fn the_login_brings_the_accounts_presentation() {
	let avatar = api::AvatarData { timestamp: 42, sign: vec![1; 64], ..Default::default() };
	let response = api::LoginSession {
		mytsid_user_cert: Some(api::MyTsUserCertificate { cert: vec![9, 8, 7] }),
		user_avatar: Some(avatar.clone()),
		..success()
	};
	let (client, _server) = mock(200, "", wire::encode(&response)).await;
	let login = client.login("mail", "password", None, "device").await.unwrap();
	assert_eq!(login.presentation.certificate, [9, 8, 7]);
	assert_eq!(login.presentation.avatar, wire::encode(&avatar));
	// Without them: nothing to show.
	let (client, _server) = mock(200, "", wire::encode(&success())).await;
	let login = client.login("mail", "password", None, "device").await.unwrap();
	assert!(login.presentation.is_empty() && login.presentation.avatar.is_empty());
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
	// What servers can be shown without a password, next to what the
	// sign-in brought: which certificate is of which kind, valid until
	// when, and verifies what. Public data only (certificates, ids, links
	// are not secret); never the session.
	if let Ok(identity) = &login.identity {
		let id = identity.id_bytes();
		let now =
			std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
				as i64;
		let own = client.own_avatar(&login.token, id).await;
		let badges = client.signed_badges(&login.token).await;
		let tag = client.user_tag(&login.token).await;
		let tag_token = client.user_tag_token(&login.token).await;
		let service = own.as_ref().ok().and_then(Option::as_ref);
		// The candidates, in the order the UI tries them: the sign-in's, the
		// avatar service's, the myTS ID's (pubSignCert).
		let certificates: [&[u8]; 3] = [
			&login.presentation.certificate,
			service.map_or(&[][..], |o| o.certificate.as_slice()),
			identity.public_signature_certificate(),
		];
		for (name, certificate) in
			["sign-in", "avatar service", "pubSignCert"].iter().zip(certificates)
		{
			// Leaf block type, validity window (Unix seconds) and public key.
			let parsed = tsproto::myts::Certificate::parse(certificate, &ROOT_KEY);
			println!(
				"{name} certificate: {} bytes, {:?}, valid now: {:?}",
				certificate.len(),
				parsed,
				parsed.as_ref().map(|c| (c.signs_myts_data(), c.valid_at(now)))
			);
		}
		println!(
			"pubSignCert equals the sign-in's: {}, the avatar service's: {}",
			certificates[2] == certificates[0],
			certificates[2] == certificates[1]
		);
		println!(
			"avatar service: {:?}, the sign-in's certificate: {:?}",
			own.as_ref().map(|o| o.is_some()),
			service.map(|o| o.certificate == login.presentation.certificate)
		);
		let avatars =
			[login.presentation.avatar.as_slice(), service.map_or(&[][..], |o| &o.avatar)];
		println!(
			"avatar (avatar, certificate): {:?}",
			choose_avatar(&avatars, &certificates, id, now)
		);
		println!(
			"badges: {:?}",
			badges.as_ref().map(|b| b.as_ref().map(|b| (
				b.iter().map(|b| b.uuid.as_str()).collect::<Vec<_>>(),
				choose_badges_certificate(&b.iter().collect::<Vec<_>>(), &certificates, id, now),
			)))
		);
		println!("tag: {:?}", tag.as_ref().map(|t| t.is_some()));
		match (&tag, &tag_token) {
			(Ok(Some(tag)), Ok(token)) => {
				let decoded = wire::decode::<api::tschat::MatrixIdentifierToken>(token.as_slice());
				let parsed = decoded
					.as_ref()
					.map(|d| tsproto::myts::Certificate::parse(&d.sign_certificate, &ROOT_KEY));
				println!("tag token certificate: {parsed:?}");
				println!("tag token: {:?}", check_user_tag(token, tag, id, now));
			}
			(_, Err(error)) => println!("tag token: {error}"),
			_ => {}
		}
	}
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

/// One GET answered with `status` and `body`; the request's head comes back.
async fn mock_get(status: u16, body: &'static [u8]) -> (String, tokio::task::JoinHandle<String>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}/avatars/a.png?sig=1", listener.local_addr().unwrap());
	let task = tokio::spawn(async move {
		let (mut stream, _) = listener.accept().await.unwrap();
		let mut request = Vec::new();
		while !request.ends_with(b"\r\n\r\n") {
			let mut byte = [0];
			stream.read_exact(&mut byte).await.unwrap();
			request.push(byte[0]);
			assert!(request.len() < 8192);
		}
		let head = format!(
			"HTTP/1.1 {status} Test\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
			body.len()
		);
		let _ = stream.write_all(head.as_bytes()).await;
		let _ = stream.write_all(body).await;
		String::from_utf8(request).unwrap()
	});
	(url, task)
}

#[tokio::test]
async fn an_avatar_is_fetched_from_its_own_link() {
	let client =
		Client { transport: transport::Transport::mock(String::new(), Duration::from_secs(2)) };
	// The file name is the link: a plain GET of it, nothing of the session.
	let (url, task) = mock_get(200, b"\x89PNG picture").await;
	assert_eq!(client.avatar(&url).await.unwrap(), b"\x89PNG picture");
	let head = task.await.unwrap();
	assert!(head.starts_with("GET /avatars/a.png?sig=1 HTTP/1.1\r\n"), "{head}");
	assert!(!head.to_ascii_lowercase().contains("authorization"), "{head}");
	// A refused download says so, apart from the account service's errors.
	let (url, _task) = mock_get(403, b"<Error><Code>AccessDenied</Code></Error>").await;
	let error = client.avatar(&url).await.unwrap_err();
	assert!(matches!(error, Error::Download(403)), "{error:?}");
	// A name that is not a link is never fetched.
	assert!(matches!(client.avatar("online.png").await, Err(Error::AvatarUrl)));
}

/// Several requests in turn, each answered with the next body; their heads
/// and bodies come back.
async fn mock_each(
	bodies: Vec<Vec<u8>>,
) -> (Client, tokio::task::JoinHandle<Vec<(String, Vec<u8>)>>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = Client {
		transport: transport::Transport::mock(
			format!("http://{}", listener.local_addr().unwrap()),
			Duration::from_secs(2),
		),
	};
	let task = tokio::spawn(async move {
		let mut requests = Vec::new();
		for body in bodies {
			let (mut stream, _) = listener.accept().await.unwrap();
			let mut head = Vec::new();
			while !head.ends_with(b"\r\n\r\n") {
				let mut byte = [0];
				stream.read_exact(&mut byte).await.unwrap();
				head.push(byte[0]);
			}
			let head = String::from_utf8(head).unwrap();
			let length: usize = head
				.lines()
				.find_map(|line| {
					line.to_ascii_lowercase().strip_prefix("content-length: ").map(str::to_owned)
				})
				.unwrap()
				.parse()
				.unwrap();
			let mut request = vec![0; length];
			stream.read_exact(&mut request).await.unwrap();
			let response = format!(
				"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
				body.len()
			);
			let _ = stream.write_all(response.as_bytes()).await;
			let _ = stream.write_all(&body).await;
			requests.push((head, request));
		}
		requests
	});
	(client, task)
}

const MYTS_ID: [u8; 33] = [4; 33];

fn session_body(method: &str) -> Vec<u8> {
	let mut body = vec![method.len() as u8];
	body.extend_from_slice(method.as_bytes());
	body.extend_from_slice(b"\x0a\x0esession-secret");
	body
}

/// The account's own avatar from the avatar service: the request as the
/// official client builds it, the entry for the account's myTS ID with
/// its certificate, as it came.
#[tokio::test]
async fn the_own_avatar_comes_from_the_avatar_service() {
	use schema_api::api::user;
	let token = SessionToken::new("session-secret").unwrap();
	let avatar = |mytsid: &[u8], cert: &[u8]| api::AvatarData {
		info: Some(api::AvatarInfo {
			map: vec![api::avatar_info::AvatarMap {
				state: api::AvatarState::Online as i32,
				name: "https://avatars.example.test/o.png".into(),
			}],
		}),
		timestamp: 42,
		sign: vec![1; 64],
		optional: Some(wire::pack_any(&api::OptionalAvatarDataContactInfo {
			mytsid: mytsid.to_vec(),
			user_cert: cert.to_vec(),
		})),
	};
	let map =
		|avatar: &api::AvatarData| user::request_contacts_avatar_info_response::AvatarInfoMap {
			id: None,
			info: Some(avatar.clone()),
		};
	let (other, own) = (avatar(&[5; 33], &[8; 112]), avatar(&MYTS_ID, &[9; 112]));
	let response = user::RequestContactsAvatarInfoResponse {
		data: vec![map(&other), map(&own)],
		error_code: 0,
	};
	let (client, server) = mock_each(vec![wire::encode(&response)]).await;
	let fetched = client.own_avatar(&token, &MYTS_ID).await.unwrap().unwrap();
	assert_eq!(fetched.certificate, [9; 112]);
	assert_eq!(fetched.avatar, wire::encode(&own), "as it came");
	let (head, body) = server.await.unwrap().remove(0);
	assert!(head.starts_with("POST /user HTTP/1.1"), "{head}");
	let type_url =
		b"type.googleapis.com/com.teamspeak.myteamspeak.proto.user.AvatarRequestID.MYTSKey";
	assert_eq!(type_url.len(), 0x50);
	let mut expected = session_body("requestContactsAvatar");
	expected.extend_from_slice(&[0x12, 0x79, 0x0a, 0x77, 0x0a, 0x50]);
	expected.extend_from_slice(type_url);
	expected.extend_from_slice(&[0x12, 0x23, 0x0a, 0x21]);
	expected.extend_from_slice(&MYTS_ID);
	assert_eq!(body, expected);
	// What the live service answers an unknown session (and an account
	// without an avatar): nothing, so nothing learned.
	let (client, _server) = mock_each(vec![Vec::new()]).await;
	assert_eq!(client.own_avatar(&token, &MYTS_ID).await.unwrap(), None);
	// Another account's entry, or one without a certificate: nothing.
	for entry in [avatar(&[5; 33], &[9; 112]), avatar(&MYTS_ID, &[]), api::AvatarData::default()] {
		let response =
			user::RequestContactsAvatarInfoResponse { data: vec![map(&entry)], error_code: 0 };
		let (client, _server) = mock_each(vec![wire::encode(&response)]).await;
		assert_eq!(client.own_avatar(&token, &MYTS_ID).await.unwrap(), None);
	}
	let expired = user::RequestContactsAvatarInfoResponse { data: vec![], error_code: 109 };
	let (client, _server) = mock_each(vec![wire::encode(&expired)]).await;
	assert!(client.own_avatar(&token, &MYTS_ID).await.unwrap_err().is_invalid_session());
}

#[tokio::test]
async fn signed_badges_keep_their_bytes() {
	use schema_api::api::user;
	let token = SessionToken::new("session-secret").unwrap();
	let badge = |uuid: &str, name: &str| user::SignedUserBadge {
		badge: Some(user::UserBadge { uuid: uuid.into(), name: name.into(), ..Default::default() }),
		sign: vec![3; 64],
		sign_timestamp: 9,
	};
	let mut odd = wire::encode(&badge("b", "B"));
	odd.extend_from_slice(&[0x78, 0x01]);
	let mut list = wire::encode(&user::UserBadgesSignedList { badges: vec![badge("a", "A")] });
	list.push(0x0a);
	list.push(odd.len() as u8);
	list.extend_from_slice(&odd);
	assert!((0x80..0x4000).contains(&list.len()));
	let mut response = vec![0x0a, 0x80 | (list.len() as u8 & 0x7f), (list.len() >> 7) as u8];
	response.extend_from_slice(&list);
	let (client, server) = mock_each(vec![response]).await;
	let badges = client.signed_badges(&token).await.unwrap().unwrap();
	assert_eq!(
		badges.iter().map(|b| (b.uuid.as_str(), b.name.as_str())).collect::<Vec<_>>(),
		[("a", "A"), ("b", "B")]
	);
	assert_eq!(badges[1].raw(), odd);
	assert_eq!(badge_list(&badges), list, "the list as it came");
	let (head, body) = server.await.unwrap().remove(0);
	assert!(head.starts_with("POST /user HTTP/1.1"), "{head}");
	assert_eq!(body, session_body("getSignedBadges"));
	assert_eq!(&body[..16], b"\x0fgetSignedBadges");
	// No list: nothing learned. An empty one: no badges.
	let (client, _server) = mock_each(vec![Vec::new()]).await;
	assert_eq!(client.signed_badges(&token).await.unwrap(), None);
	let (client, _server) = mock_each(vec![vec![0x0a, 0x00]]).await;
	assert_eq!(client.signed_badges(&token).await.unwrap(), Some(Vec::new()));
	// What the live service answers an unknown session.
	let (client, _server) = mock_each(vec![vec![0x10, 0x6d]]).await;
	assert!(client.signed_badges(&token).await.unwrap_err().is_invalid_session());
}

/// The User Tag comes from the chat service with the session alone.
#[tokio::test]
async fn the_user_tag_and_its_token_come_from_the_chat_service() {
	use schema_api::api::{tschat, user};
	let token = SessionToken::new("session-secret").unwrap();
	let list = tschat::TschatIdentifierList {
		ts_chat_identifier_mapping: vec![tschat::TschatIdentifierMapping {
			ts_chat_identifier: "alex@myteamspeak.com".into(),
			matrix_id: "@x:tschat".into(),
			primary: true,
		}],
		error_handling: Some(tschat::ErrorHandling { return_code: 1, ..Default::default() }),
	};
	let (client, server) = mock_each(vec![wire::encode(&list)]).await;
	assert_eq!(client.user_tag(&token).await.unwrap().as_deref(), Some("alex@myteamspeak.com"));
	let (head, body) = server.await.unwrap().remove(0);
	assert!(head.starts_with("POST /tschat HTTP/1.1"), "{head}");
	assert_eq!(body, session_body("getActiveIdentifierList"));
	assert_eq!(body[0], 0x17);
	// No primary one: the account data's active identifiers.
	let data = user::UserAccountData {
		ts_chat_identifier_list_active: Some(list.clone()),
		..Default::default()
	};
	let none = tschat::TschatIdentifierList::default();
	let (client, server) = mock_each(vec![wire::encode(&none), wire::encode(&data)]).await;
	assert_eq!(client.user_tag(&token).await.unwrap().as_deref(), Some("alex@myteamspeak.com"));
	let requests = server.await.unwrap();
	assert!(requests[1].0.starts_with("POST /user HTTP/1.1"));
	assert!(requests[1].1.starts_with(b"\x0egetAccountData"));
	let request: user::AccountDataRequest = wire::decode(&requests[1].1[15..]).unwrap();
	assert_eq!(request.selector, [user::UserAccountDataSelector::TsChat as i32]);
	// An expired session is the account service's expired session.
	let expired = tschat::TschatIdentifierList {
		error_handling: Some(tschat::ErrorHandling { return_code: 5, ..Default::default() }),
		..Default::default()
	};
	let (client, _server) = mock_each(vec![wire::encode(&expired)]).await;
	assert!(client.user_tag(&token).await.unwrap_err().is_invalid_session());
	// The token, exactly as it came.
	let mut raw = wire::encode(&tschat::MatrixIdentifierToken {
		signature: vec![1; 64],
		sign_certificate: vec![2; 112],
		timestamp: 5,
		tags: Some(tschat::TschatIdentifierTagList { tag: vec!["alex@myteamspeak.com".into()] }),
	});
	raw.extend_from_slice(&[0x78, 0x01]);
	assert!((0x80..0x4000).contains(&raw.len()));
	let mut answer = vec![0x0a, 0x80 | (raw.len() as u8 & 0x7f), (raw.len() >> 7) as u8];
	answer.extend_from_slice(&raw);
	answer.extend_from_slice(&wire::encode(&tschat::SignedAllowedIdentifier {
		token: None,
		error: Some(tschat::ErrorHandling { return_code: 1, ..Default::default() }),
	}));
	let (client, server) = mock_each(vec![answer]).await;
	assert_eq!(client.user_tag_token(&token).await.unwrap(), raw);
	let (head, body) = server.await.unwrap().remove(0);
	assert!(head.starts_with("POST /tschat HTTP/1.1"), "{head}");
	assert_eq!(body, session_body("requestSignedAllowedIdentifierList"));
	assert_eq!(body[0], 0x22);
	// The live answer to an unknown session: SESSION_EXPIRED (5).
	let mut live = vec![0x12, 0x44, 0x08, 0x05, 0x12, 0x40];
	live.extend_from_slice(b"Could not resolve session '00000000-0000-0000-0000-000000000000'");
	assert_eq!(live.len(), 6 + 0x40);
	let (client, _server) = mock_each(vec![live]).await;
	assert!(client.user_tag_token(&token).await.unwrap_err().is_invalid_session());
}
