//! Runtime schema inspection, dynamic protobuf messages, and protobuf JSON.
//!
//! The JSON helpers use the protobuf JSON mapping: field names are lower camel
//! case, byte fields are base64, 64-bit integers are strings, and enums use their
//! schema names. Embedded `google.protobuf.Any` values are resolved against the
//! bundled descriptors. Unknown JSON fields are rejected by default.

use std::sync::OnceLock;

use prost::{Message, Name};
use prost_reflect::ReflectMessage;
pub use prost_reflect::{
    DescriptorPool, DeserializeOptions, DynamicMessage, MessageDescriptor, SerializeOptions,
};

/// An error resolving, decoding, or converting a protobuf message.
#[derive(Debug, thiserror::Error)]
pub enum ReflectionError {
    /// The bundled descriptor set does not contain the requested message.
    #[error("protobuf message `{0}` is not present in the bundled descriptors")]
    UnknownMessage(String),
    /// A dynamic message was converted to a different protobuf message type.
    #[error("protobuf message type mismatch: expected `{expected}`, got `{actual}`")]
    TypeMismatch {
        /// Fully qualified protobuf name of the requested Rust type.
        expected: String,
        /// Fully qualified protobuf name of the dynamic message.
        actual: String,
    },
    /// The protobuf wire representation could not be decoded.
    #[error("cannot decode protobuf message `{message_name}`: {source}")]
    Decode {
        /// Fully qualified protobuf message name.
        message_name: String,
        /// Original protobuf decoding error.
        #[source]
        source: prost::DecodeError,
    },
    /// Protobuf JSON encoding or decoding failed.
    #[error("protobuf JSON conversion failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Returns the shared descriptors for all bundled application schemas and imports.
///
/// Initialize once, then reuse this pool for message, service, method, field, and
/// enum inspection. This does not contact a server or enable a reflection RPC.
///
/// # Panics
///
/// Panics if the descriptor set embedded when this crate was built is invalid.
pub fn descriptor_pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(crate::FILE_DESCRIPTOR_SET)
            .expect("the descriptor set embedded at build time must be valid")
    })
}

/// Looks up a fully qualified protobuf message name, with an optional leading dot.
pub fn message_descriptor(full_name: &str) -> Result<MessageDescriptor, ReflectionError> {
    let name = full_name.strip_prefix('.').unwrap_or(full_name);
    descriptor_pool()
        .get_message_by_name(name)
        .ok_or_else(|| ReflectionError::UnknownMessage(full_name.to_owned()))
}

/// Decodes protobuf wire bytes using the message's fully qualified schema name.
///
/// Unknown wire fields are retained in the returned dynamic message. Conversion
/// to generated Rust types or JSON may discard those unknown fields.
pub fn decode_dynamic(
    full_name: &str,
    bytes: impl AsRef<[u8]>,
) -> Result<DynamicMessage, ReflectionError> {
    let descriptor = message_descriptor(full_name)?;
    let message_name = descriptor.full_name().to_owned();
    DynamicMessage::decode(descriptor, bytes.as_ref()).map_err(|source| ReflectionError::Decode {
        message_name,
        source,
    })
}

/// Converts a generated Rust message into a dynamic message using its schema name.
pub fn to_dynamic<M: Message + Name>(message: &M) -> Result<DynamicMessage, ReflectionError> {
    decode_dynamic(&M::full_name(), message.encode_to_vec())
}

/// Converts a dynamic message to its corresponding generated Rust type.
///
/// The fully qualified protobuf names must match; a wire-compatible but different
/// type is rejected. Unknown wire fields are discarded by generated Rust types.
pub fn from_dynamic<M: Message + Name + Default>(
    message: &DynamicMessage,
) -> Result<M, ReflectionError> {
    let expected = M::full_name();
    let actual = message.descriptor().full_name().to_owned();
    if actual != expected {
        return Err(ReflectionError::TypeMismatch { expected, actual });
    }
    message
        .transcode_to()
        .map_err(|source| ReflectionError::Decode {
            message_name: expected,
            source,
        })
}

/// Serializes a generated message using the standard protobuf JSON mapping.
pub fn to_json<M: Message + Name>(message: &M) -> Result<String, ReflectionError> {
    Ok(serde_json::to_string(&to_dynamic(message)?)?)
}

/// Serializes a generated message as pretty-printed protobuf JSON.
pub fn to_json_pretty<M: Message + Name>(message: &M) -> Result<String, ReflectionError> {
    Ok(serde_json::to_string_pretty(&to_dynamic(message)?)?)
}

/// Serializes a generated message with explicitly chosen protobuf JSON options.
///
/// Some options, such as emitting numeric 64-bit values, depart from the standard
/// mapping. Use [`to_json`] for the standard representation.
pub fn to_json_with_options<M: Message + Name>(
    message: &M,
    options: &SerializeOptions,
) -> Result<String, ReflectionError> {
    let mut bytes = Vec::new();
    to_dynamic(message)?
        .serialize_with_options(&mut serde_json::Serializer::new(&mut bytes), options)?;
    // A JSON serializer produces valid UTF-8 by construction.
    Ok(String::from_utf8(bytes).expect("JSON output must be valid UTF-8"))
}

/// Parses one complete protobuf JSON document into a generated message.
///
/// Both schema field names and their JSON names are accepted. Unknown fields,
/// unknown symbolic enum names, and trailing content are rejected.
pub fn from_json<M: Message + Name + Default>(json: &str) -> Result<M, ReflectionError> {
    from_json_with_options(json, &DeserializeOptions::new())
}

/// Parses protobuf JSON with explicit decoding options.
pub fn from_json_with_options<M: Message + Name + Default>(
    json: &str,
    options: &DeserializeOptions,
) -> Result<M, ReflectionError> {
    from_dynamic(&dynamic_from_json_with_options(
        &M::full_name(),
        json,
        options,
    )?)
}

/// Parses protobuf JSON into a dynamic message identified by its schema name.
pub fn dynamic_from_json(full_name: &str, json: &str) -> Result<DynamicMessage, ReflectionError> {
    dynamic_from_json_with_options(full_name, json, &DeserializeOptions::new())
}

/// Parses protobuf JSON into a dynamic message with explicit decoding options.
pub fn dynamic_from_json_with_options(
    full_name: &str,
    json: &str,
    options: &DeserializeOptions,
) -> Result<DynamicMessage, ReflectionError> {
    let descriptor = message_descriptor(full_name)?;
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let message = DynamicMessage::deserialize_with_options(descriptor, &mut deserializer, options)?;
    deserializer.end()?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{LoginData, LoginSession};
    use serde_json::json;

    #[test]
    fn descriptors_are_cached_and_include_service_methods() {
        assert!(std::ptr::eq(descriptor_pool(), descriptor_pool()));
        let service = descriptor_pool()
            .get_service_by_name("com.teamspeak.myteamspeak.proto.login.LoginService")
            .unwrap();
        let method = service
            .methods()
            .find(|method| method.name() == "login")
            .unwrap();
        assert_eq!(method.input().full_name(), LoginData::full_name());
        assert_eq!(method.output().full_name(), LoginSession::full_name());
    }

    #[test]
    fn json_uses_schema_enum_names_and_camel_case_fields() {
        let message = LoginData {
            email: "person@example.invalid".into(),
            skip_session: true,
            origin: 1,
            device_id: "device-1".into(),
            ..Default::default()
        };
        let value: serde_json::Value = serde_json::from_str(&to_json(&message).unwrap()).unwrap();
        assert_eq!(
            value,
            json!({
                "email": "person@example.invalid",
                "skipSession": true,
                "origin": "WEBSITE",
                "deviceId": "device-1",
            })
        );
        assert_eq!(from_json::<LoginData>(&value.to_string()).unwrap(), message);
    }

    #[test]
    fn json_encodes_bytes_and_full_precision_int64() {
        let message = LoginSession {
            key: vec![0, 1, 2, 255],
            purge: i64::MAX,
            error: 200,
            ..Default::default()
        };
        let value: serde_json::Value = serde_json::from_str(&to_json(&message).unwrap()).unwrap();
        assert_eq!(value["key"], "AAEC/w==");
        assert_eq!(value["purge"], "9223372036854775807");
        assert_eq!(value["error"], "ERROR_LOGIN_OK");
        assert_eq!(
            from_json::<LoginSession>(&value.to_string()).unwrap(),
            message
        );
    }

    #[test]
    fn unknown_numeric_enum_values_survive_json() {
        let message: LoginData = from_json(r#"{"origin":9237}"#).unwrap();
        assert_eq!(message.origin, 9237);
        assert_eq!(to_json(&message).unwrap(), r#"{"origin":9237}"#);
    }

    #[test]
    fn parser_accepts_original_field_names() {
        let message: LoginData = from_json(r#"{"device_id":"device-2"}"#).unwrap();
        assert_eq!(message.device_id, "device-2");
    }

    #[test]
    fn invalid_json_is_rejected_without_ignoring_trailing_content() {
        for input in [
            r#"{"origin":"UNDEFINED_ORIGIN"}"#,
            r#"{"email":false}"#,
            r#"{} {}"#,
            r#"{"unknownField":true}"#,
        ] {
            assert!(from_json::<LoginData>(input).is_err(), "accepted {input}");
        }
        assert!(from_json::<LoginSession>(r#"{"purge":"9223372036854775808"}"#).is_err());
        assert!(from_json::<LoginSession>(r#"{"key":"%%%"}"#).is_err());
    }

    #[test]
    fn caller_can_explicitly_accept_unknown_json_fields() {
        let message = from_json_with_options::<LoginData>(
            r#"{"email":"test@example.invalid","future":true}"#,
            &DeserializeOptions::new().deny_unknown_fields(false),
        )
        .unwrap();
        assert_eq!(message.email, "test@example.invalid");
    }

    #[test]
    fn dynamic_decoding_preserves_unknown_wire_fields() {
        // LoginData.email = "a" followed by unknown varint field 100 = 1.
        let wire = [0x0a, 0x01, b'a', 0xa0, 0x06, 0x01];
        let dynamic = decode_dynamic(&format!(".{}", LoginData::full_name()), wire).unwrap();
        assert_eq!(dynamic.unknown_fields().count(), 1);
        assert_eq!(dynamic.encode_to_vec(), wire);
        let typed: LoginData = from_dynamic(&dynamic).unwrap();
        assert_eq!(typed.email, "a");
        assert_eq!(typed.encode_to_vec(), [0x0a, 0x01, b'a']);
    }

    #[test]
    fn dynamic_conversion_checks_message_identity() {
        let dynamic = to_dynamic(&LoginData::default()).unwrap();
        assert!(matches!(
            from_dynamic::<LoginSession>(&dynamic),
            Err(ReflectionError::TypeMismatch { .. })
        ));
        assert!(matches!(
            decode_dynamic("missing.Message", []),
            Err(ReflectionError::UnknownMessage(_))
        ));
        assert!(matches!(
            decode_dynamic(&LoginData::full_name(), [0x0a, 0xff]),
            Err(ReflectionError::Decode { .. })
        ));
    }

    #[test]
    fn push_any_payload_uses_embedded_descriptors() {
        let input = json!({
            "payload": {
                "@type": "type.googleapis.com/com.teamspeak.myteamspeak.proto.push.AuthTokenUsed",
                "idHash": "hash-123"
            }
        });
        let dynamic = dynamic_from_json(
            "com.teamspeak.myteamspeak.proto.push.PushNotification",
            &input.to_string(),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&dynamic).unwrap(), input);
        let message: crate::api::push::PushNotification = from_dynamic(&dynamic).unwrap();
        assert_eq!(
            from_json::<crate::api::push::PushNotification>(&input.to_string()).unwrap(),
            message
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&to_json(&message).unwrap()).unwrap(),
            input
        );
        let payload = message.payload.unwrap();
        assert_eq!(
            payload.type_url,
            "type.googleapis.com/com.teamspeak.myteamspeak.proto.push.AuthTokenUsed"
        );
        let decoded = crate::api::push::AuthTokenUsed::decode(payload.value.as_slice()).unwrap();
        assert_eq!(decoded.id_hash, "hash-123");
    }
}
