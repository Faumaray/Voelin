use schema_api::{account, api, prost::Message};

#[test]
fn session_matches_original_wire_field() {
    let message = api::Session {
        session: "abc".into(),
    };
    assert_eq!(message.encode_to_vec(), [0x0a, 3, b'a', b'b', b'c']);
    assert_eq!(api::Session::decode(&b"\x0a\x03abc"[..]).unwrap(), message);
}

#[test]
fn proto2_presence_survives_encoding() {
    let absent = account::AccountSession::default();
    let present = account::AccountSession {
        id: Some(String::new()),
    };
    assert!(absent.encode_to_vec().is_empty());
    assert_eq!(present.encode_to_vec(), [0x0a, 0]);
    assert_eq!(
        account::AccountSession::decode(present.encode_to_vec().as_slice())
            .unwrap()
            .id,
        Some(String::new())
    );
}

#[test]
fn unknown_enum_values_survive_binary_roundtrip() {
    let message = api::LoginStatus {
        error: 654321,
        uuid: String::new(),
    };
    let decoded = api::LoginStatus::decode(message.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.error, 654321);
    assert!(api::ErrorCommon::try_from(decoded.error).is_err());
}

#[cfg(feature = "reflection")]
#[test]
fn all_application_services_and_methods_are_present() {
    let pool = schema_api::reflection::descriptor_pool();
    let expected = [
        ("com.teamspeak.myteamspeak.proto.login.LoginService", 12),
        (
            "com.teamspeak.myteamspeak.proto.user.UserAccountService",
            30,
        ),
        (
            "com.teamspeak.myteamspeak.proto.management.user.UserManagementService",
            39,
        ),
        (
            "com.teamspeak.myteamspeak.proto.integration.IntegrationUserService",
            4,
        ),
        (
            "com.teamspeak.myteamspeak.proto.synchronization.SynchronizationService",
            2,
        ),
        ("com.teamspeak.myteamspeak.proto.tschat.ChatRequests", 16),
        ("com.teamspeak.myteamspeak.proto.addon.UserAddonService", 1),
        (
            "com.teamspeak.myteamspeak.proto.messengerconnector.MessengerConnectorClientService",
            1,
        ),
        ("com.teamspeak.push.proto.PushService", 1),
    ];
    let mut unary = 0;
    let mut streaming = 0;
    for (name, count) in expected {
        let service = pool.get_service_by_name(name).expect(name);
        assert_eq!(service.methods().count(), count, "{name}");
        for method in service.methods() {
            assert!(!method.is_client_streaming());
            if method.is_server_streaming() {
                streaming += 1;
            } else {
                unary += 1;
            }
        }
    }
    assert_eq!((unary, streaming), (105, 1));
}
