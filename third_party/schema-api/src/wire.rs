//! Protobuf binary encoding, checked `Any` envelopes, and opt-in push decoding.

use prost::{Message, Name};

/// Serialize a protobuf message to its ordinary, non-length-delimited bytes.
pub fn encode<M: Message>(message: &M) -> Vec<u8> {
    message.encode_to_vec()
}

/// Decode ordinary protobuf bytes into a caller-selected message type.
///
/// Protobuf itself is not self-describing. A successful decode does not prove
/// that the sender intended the selected type; use [`unpack_any`] when an `Any`
/// envelope is available.
pub fn decode<M: Message + Default>(bytes: impl AsRef<[u8]>) -> Result<M, prost::DecodeError> {
    M::decode(bytes.as_ref())
}

/// Pack a generated message with the conventional `type.googleapis.com` prefix.
pub fn pack_any<M: Message + Name>(message: &M) -> prost_types::Any {
    pack_any_with_prefix(message, "type.googleapis.com")
}

/// Pack a message using a caller-supplied type URL prefix.
///
/// Trailing slashes on `prefix` are normalized. The prefix is not dereferenced
/// or validated as a network address. The final URL segment is the fully
/// qualified protobuf message name supplied by [`Name`].
pub fn pack_any_with_prefix<M: Message + Name>(message: &M, prefix: &str) -> prost_types::Any {
    prost_types::Any {
        type_url: format!("{}/{}", prefix.trim_end_matches('/'), M::full_name()),
        value: encode(message),
    }
}

/// Failure to unpack an `Any` envelope.
#[derive(Debug, thiserror::Error)]
pub enum AnyError {
    /// The type URL did not contain `/` followed by the expected full name.
    #[error("Any type URL {actual:?} does not identify {expected}")]
    TypeMismatch {
        /// The fully qualified protobuf name expected by the caller.
        expected: String,
        /// The unmodified type URL from the envelope.
        actual: String,
    },
    /// The name matched, but the binary message could not be decoded.
    #[error("invalid Any payload: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Check the full protobuf type name before decoding an `Any` envelope.
///
/// Custom URL prefixes are accepted, as required by `google.protobuf.Any`:
/// only the segment after the last `/` identifies the message type. A bare
/// message name without `/` is rejected. No URL is fetched.
pub fn unpack_any<M: Message + Name + Default>(any: &prost_types::Any) -> Result<M, AnyError> {
    let expected = M::full_name();
    if any.type_url.rsplit_once('/').map(|(_, name)| name) != Some(expected.as_str()) {
        return Err(AnyError::TypeMismatch {
            expected,
            actual: any.type_url.clone(),
        });
    }
    Ok(decode(&any.value)?)
}

/// Decode a push transport payload as a message type selected by the caller.
///
/// `PushService.Message.payload` is opaque `bytes`: the supplied schema gives
/// no framing, compression, encryption, or payload-type contract. Use this
/// helper only when your application defines the payload as ordinary protobuf
/// bytes of `M`. Unknown protobuf fields can make an incorrect type appear to
/// decode successfully, so decoding is not a type check.
pub fn decode_push_payload<M: Message + Default>(
    message: &crate::push::Message,
) -> Result<M, prost::DecodeError> {
    decode(&message.payload)
}

/// Opt in to treating push bytes as an application `PushNotification` envelope.
///
/// This relationship is **not guaranteed by the schemas**. Call this only when
/// your server specifies it. Then use [`unpack_any`] on the notification's
/// optional `payload` to validate and decode its concrete message type.
pub fn decode_push_notification(
    message: &crate::push::Message,
) -> Result<crate::api::push::PushNotification, prost::DecodeError> {
    decode_push_payload(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;

    #[test]
    fn any_roundtrip_accepts_custom_prefix_but_checks_the_type_name() {
        let message = api::Session {
            session: "token".into(),
        };
        let any = pack_any_with_prefix(&message, "https://schema.example.test/types/");
        assert_eq!(
            any.type_url,
            "https://schema.example.test/types/com.teamspeak.myteamspeak.proto.Session"
        );
        assert_eq!(unpack_any::<api::Session>(&any).unwrap(), message);
        // This other Session has identical wire fields, but a different full
        // protobuf name. Decoding bytes alone would not catch that mistake.
        assert!(matches!(
            unpack_any::<api::user::Session>(&any),
            Err(AnyError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn malformed_envelopes_and_payloads_are_rejected() {
        let mut any = pack_any(&api::Session::default());
        any.type_url = api::Session::full_name();
        assert!(matches!(
            unpack_any::<api::Session>(&any),
            Err(AnyError::TypeMismatch { .. })
        ));

        any.type_url = format!("type.googleapis.com/{}", api::Session::full_name());
        any.value = vec![0x0a, 0x02, 0x41]; // string claims two bytes, only one follows.
        assert!(matches!(
            unpack_any::<api::Session>(&any),
            Err(AnyError::Decode(_))
        ));
    }

    #[test]
    fn opt_in_push_decoding_supports_checked_nested_any() {
        let notification = api::push::SimpleNotification {
            r#type: 1,
            relogin_neccessary: true,
        };
        let envelope = api::push::PushNotification {
            payload: Some(pack_any(&notification)),
        };
        let push_message = crate::push::Message {
            payload: encode(&envelope),
        };
        let decoded = decode_push_notification(&push_message).unwrap();
        assert_eq!(
            unpack_any::<api::push::SimpleNotification>(&decoded.payload.unwrap()).unwrap(),
            notification,
        );
    }
}
