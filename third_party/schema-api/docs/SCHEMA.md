# Source schema audit and RPC catalog

This document audits the supplied archive only. The namespace spelling is retained as a wire contract; it does not establish a relationship with any external service. No endpoint, credential, live server, or implementation documentation was supplied.

## Coverage and provenance

The archive contains **123 protobuf files**: **23 application files** and **100 supporting infrastructure files**. The application schemas define **228 explicitly declared messages**, **33 enums**, and **9 services with 106 methods**. Of those methods, **105 are unary** and **one is server streaming**. There are no application client-streaming or bidirectional methods. Message counts include nested declarations and exclude synthetic protobuf map-entry messages. The machine-readable [inventory](schema-inventory.json) records every source file, SHA-256 digest, package, message, enum value, and RPC path.

Every textual import points to a file in the archive. This is an import-completeness check, not proof that the entire archive compiles. Application source files contain no comments explaining protocol behavior. The only nonapplication imports needed by the application files are `google/protobuf/any.proto` and `google/protobuf/empty.proto`.

### Application source files

| File | Package | Messages | Enums | RPCs |
| --- | --- | ---: | ---: | ---: |
| `Account_Serializing.proto` | `com.teamspeak.account.proto` | 8 | 0 | 0 |
| `Sync_Serializing.proto` | `com.teamspeak.sync.proto` | 16 | 6 | 0 |
| `blacklist_info.proto` | `com.teamspeak.blacklist` | 2 | 1 | 0 |
| `client_lib_cache.proto` | `com.teamspeak.proto.client_cache` | 1 | 0 | 0 |
| `global_badge_list.proto` | `com.teamspeak.myteamspeak.proto.cloud` | 2 | 0 | 0 |
| `myteamspeak_addon.proto` | `com.teamspeak.myteamspeak.proto.addon` | 4 | 4 | 1 |
| `myteamspeak_avatar_common.proto` | `com.teamspeak.myteamspeak.proto` | 4 | 2 | 0 |
| `myteamspeak_common.proto` | `com.teamspeak.myteamspeak.proto` | 19 | 3 | 0 |
| `myteamspeak_integration_common.proto` | `com.teamspeak.myteamspeak.proto.integration` | 7 | 4 | 0 |
| `myteamspeak_integration_static_info.proto` | `com.teamspeak.myteamspeak.proto.integration` | 3 | 1 | 0 |
| `myteamspeak_integration_user.proto` | `com.teamspeak.myteamspeak.proto.integration` | 11 | 0 | 4 |
| `myteamspeak_login.proto` | `com.teamspeak.myteamspeak.proto.login` | 12 | 0 | 12 |
| `myteamspeak_messenger_connector_client.proto` | `com.teamspeak.myteamspeak.proto.messengerconnector` | 5 | 1 | 1 |
| `myteamspeak_namedserver_common.proto` | `com.teamspeak.myteamspeak.proto.namedserver` | 4 | 0 | 0 |
| `myteamspeak_push.proto` | `com.teamspeak.myteamspeak.proto.push` | 6 | 1 | 0 |
| `myteamspeak_synchronization.proto` | `com.teamspeak.myteamspeak.proto.synchronization` | 0 | 0 | 2 |
| `myteamspeak_tschat.proto` | `com.teamspeak.myteamspeak.proto.tschat` | 19 | 1 | 16 |
| `myteamspeak_user.proto` | `com.teamspeak.myteamspeak.proto.user` | 55 | 3 | 30 |
| `myteamspeak_user_management.proto` | `com.teamspeak.myteamspeak.proto.management.user` | 43 | 3 | 39 |
| `revocation_list.proto` | `com.teamspeak.revocation_list` | 1 | 0 | 0 |
| `revoke_cache.proto` | `com.teamspeak.proto.revocation` | 1 | 0 | 0 |
| `synchronization_common.proto` | `com.teamspeak.myteamspeak.proto.synchronization` | 4 | 3 | 0 |
| `teamspeak_push_grpc.proto` | `com.teamspeak.push.proto` | 1 | 0 | 1 |

`Account_Serializing.proto` and `Sync_Serializing.proto` use proto2. All other application files use proto3. The account/sync serialization, badge-list, named-server, revocation, blacklist and client-cache files define data formats without adding service methods. These formats are useful to the crate even though they do not establish a transport endpoint.

## Authentication and sessions

Most authenticated requests carry a session string **inside the protobuf body**. The shared `com.teamspeak.myteamspeak.proto.Session` and the distinct `com.teamspeak.myteamspeak.proto.user.Session` have equivalent fields but remain distinct types. Some requests nest the session: `NewUserPublicKeyData.session.session`, chat `user.session`, `UpdateBadgeRequest.addBadgeRequest.session`, and community listing `request.session`.

`LoginData` supports email/password plus `otp`, `device_id`, `otp_renewal_token`, `device_name`, `skipSession`, login origin and sync version. Login results include the session, key, UUID, limits, identity material, permissions, push token and optional alternative-login data. The latter contains renewal token, OTP renewal token and device ID. Auth-token and renewal-token login methods are also explicitly defined. Do not infer password hashing, key derivation, renewal timing, automatic token refresh, or a relation between `purge` and local clock units.

No schema declares an authorization metadata header, bearer-token format, endpoint, TLS requirement, hostname, or service routing layout. A client may expose custom metadata and separate service channels, but must not assume a particular header or silently copy the session/push token into one. Several management and account-status requests contain no session or credential field; this does not prove that the corresponding methods are unauthenticated. Such access may depend on deployment-specific transport metadata or service authorization.

Session convenience helpers should populate only explicit session fields and retain raw generated request access. Login helpers should preserve the full reply rather than discarding permissions, identity data or alternative-login fields. Keep passwords, renewal tokens, sessions, private keys, and OTP secrets out of diagnostic logs; generated message debugging can contain these values.

## Application status semantics

A successful gRPC transport status does not imply application success. Preserve the original response and unknown numeric enum values so callers can handle server extensions.

| Domain / enum | Evidence in schema | Client treatment |
| --- | --- | --- |
| Common `ErrorCommon` | Session OK `102`, login OK `200`, session deleted OK `204`; OTP required `208`; expired session `103` | Check the success appropriate to the RPC; expose OTP-required as a distinct outcome. `UNKNOWN_ERRORCOMMON = 0` is not success. |
| User `ErrorReturnCode` | Unknown `0`, named failures `100..132`; no named success value | Where a `success` boolean exists, use it. For enum-only responses, report the raw code; zero is unspecified rather than proof of success. |
| Management `UserManagementErrorReturnCode` | Unknown `0`, named failures `100..139`; no named success value | Same ambiguity as user responses. Preserve textual error and boolean fields when present. |
| Integration `IntegrationError` | OK `1`; session expired `8`; pending `10`; missing information `7`; invalid timestamp `11` | Success is `1`; retain `seconds_to_wait` on subscription replies without inventing a retry policy. |
| Avatar `AvatarError` | OK `1`, critical `2`, invalid session `3` | Success is `1`. |
| Chat `ChatRequestReturnCode` | Success `1`; connection `2`, internal `3`, request `4`, expired session `5`, account creation disabled `6` | Success is `1`; preserve nested `ErrorHandling.message`. |
| Messenger `MessengerConnectorReturnCodesClient` | Account created `1`; creation failed `100`, already exists `101` | Success is `1`; do not automatically treat already-exists as success. |
| Synchronization `SyncStatus` | In sync `300`, not in sync `301`, not in database `302`, collision `303`, over limit `304` | These are synchronization outcomes, not a generic boolean. Return them without automatic merging or retrying writes. |
| Add-on `ReturnCode` | Not found `1`, platform not found `2`, up to date `3`, update `4`, group `5` | Return a typed outcome; both up-to-date and update are useful results, with different payload interpretation. |
| Login management replies | `AuthTokenExpireReply` / `AccountStatusReply` have `success` and `error_msg`; token-list and suspension-reason replies have no application status | Use the declared boolean where available; absence of an error field must not be filled by guessed status codes. |

Response status fields vary in spelling and nesting (`error`, `error_code`, `errorcode`, `returncode`, `return_code`, `error_handling`). A universal reflective rule such as “an enum field named error must equal zero” is incorrect. A missing optional nested return-code message is also distinct from an explicit success.

## Streaming and opaque payloads

`com.teamspeak.push.proto.PushService/longPull` accepts `google.protobuf.Empty` and streams `Message { bytes payload = 1; }`. The request has no session or token field. Cancellation follows stream drop/cancellation at the transport layer. The schema provides no replay cursor, acknowledgment, reconnect backoff, heartbeat, or resume semantics; reconnecting cannot promise lossless delivery.

A different package defines `push.PushNotification` and `push.IntermediateData`, each with an `Any` payload, plus `SimpleNotification`, `ContactAvatarChanged`, `TsChatAvatarChanged` and `AuthTokenUsed`. The schema does **not** say whether the bytes emitted by `longPull` contain either of those envelopes, compressed data, or another encoding. Make decoding explicit and fallible. Support standard protobuf `Any` type URLs and preserve unknown payloads; do not choose an envelope by trial decoding alone, because protobuf accepts many unrelated byte sequences.

The `SimpleNotification` enum distinguishes sync-data changes (`1`), session expiry (`2`), and contact-homebase changes (`3`). `relogin_neccessary` is an independent boolean. Nothing in the schema specifies whether to obey it automatically or how to rebuild application state.

## Synchronization, identities and transfers

Synchronization requests carry the session, class/version list, global version and sync version. Each item detail has UUID, item version, opaque blob and the field `item_delte_on_server`. The data-serialization schema defines bookmarks, identities, hotkey profiles, whispers, add-ons, configuration, folders, group chats and contacts. Its `Item_Data.item_content` oneof permits typed local decoding, but the schema does not state that network `item_blob` is plain `Item_Data` rather than encrypted or wrapped. Expose codecs without automatic encryption/decryption assumptions. Preserve class/version IDs and collisions; never overwrite conflicting data without caller policy.

Signed identity, avatar, badge, integration, and Matrix identifier structures expose key, certificate, signature and timestamp bytes. They do not identify a signing algorithm, canonical signed bytes, certificate chain policy, signature verification procedure, or timestamp unit. Returning these structures is supported; inventing cryptographic verification is not.

Avatar/file RPCs return signed URLs and metadata. The schemas do not state the HTTP upload method, required headers, multipart layout, checksum policy, or URL expiration. Requesting an upload/download URL is part of the gRPC client surface. Transferring bytes through the URL requires deployment-specific HTTP details and should not be guessed.

## Schema quality and compilation cautions

- **Malformed bundled Google option syntax:** the supplied `google/protobuf/descriptor.proto`, beginning around line 358, serializes repeated/message-valued options with invalid source syntax, for example `targets = TARGET_TYPE_FIELD, TARGET_TYPE_FILE` and `edition_defaults = [value: ...]`. `google/protobuf/cpp_features.proto` has the same issue. Prefer compatible compiler-vendored standard protobuf includes when compiling optional infrastructure, and retain the supplied files as original source material. The application schemas do not need either malformed file. This finding is a source inspection result; compiler validation results belong to the crate's verification log.
- **Mixed presence semantics:** proto2 optional scalars must remain optional. For example, an absent bookmark `send_mytsid_on_server` has declared default `true`; reading its raw `Option<bool>` as `false` would change behavior. Proto3 scalars generally do not record whether an explicit default was sent. Message fields and oneofs retain presence.
- **Preserve original spelling:** wire/source symbols include `ERRIR_RENEWAL_TOKEN_INVALID`, `item_delte_on_server`, `relogin_neccessary`, lowercase `requestExpireTime`/`replyExpireTime`, mixed field casing and `Status_public_license_id`. Idiomatic Rust accessors may differ, but field numbers, enum numbers, full protobuf names and RPC paths must remain exact.
- **Reserved and deprecated fields:** respect declared reserved numbers; deprecated OTP QR-code bytes/MIME fields remain part of the wire format. Do not delete them from generated bindings.
- **Unknown enums and fields:** expose enum integers when no named variant is known. Prost generated messages generally discard unknown protobuf fields when decoded and re-encoded; use original bytes or a suitable dynamic representation when lossless forwarding matters.
- **No validation contract:** the archive includes `validate.proto` definitions, but the application schemas do not attach validation rules. Required business fields, string formats, UUID validation, quotas, size limits, and timestamp units are unspecified. Avoid rejecting otherwise encodable requests based on guessed rules.
- **Mutating operations and retries:** RPC names include account deletion, reset, badge management, key updates and voice-server changes, but there are no application idempotency annotations or request IDs. Retrying arbitrary failed RPCs can repeat side effects. Expose timeouts and caller-controlled retries.
- **Infrastructure is not application functionality:** the other 100 files describe Google protobuf/RPC/CEL, Envoy, xDS, UDPA, validation and gRPC support types. Their only additional service is Envoy load reporting. An optional infrastructure feature can expose these bindings without treating them as ordinary application API endpoints.

## Application RPC catalog

All method names and request/response names below are the original protobuf spelling. Request/response cells use names as declared in the source; shared parent-package types resolve by protobuf lexical scoping. The JSON inventory additionally records fully resolved type names. Each section states the full service name; the gRPC path is `/<full service name>/<method>`. “Body fields” lists evident session/credential fields, not a claim about required authorization. “None declared” never means public access.

### `com.teamspeak.myteamspeak.proto.addon.UserAddonService`

Source: `myteamspeak_addon.proto`. 1 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `requestDownload` | `GetDownload` | `Download` | None declared |

### `com.teamspeak.myteamspeak.proto.integration.IntegrationUserService`

Source: `myteamspeak_integration_user.proto`. 4 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `getIntegrationUserStatus` | `RequestIntegrationUserStatus` | `IntegrationStatusUserResponse` | `session` |
| `requestBindIntegration` | `RequestBindData` | `RequestBindResponse` | `session` |
| `requestUnbindIntegration` | `RequestUnbindData` | `RequestBindResponse` | `session` |
| `getUserIntegrationSubscriptionInfo` | `UserIntegrationSubscriptionInfoRequest` | `UserIntegrationSubscriptionInfoResponse` | `session_id` |

### `com.teamspeak.myteamspeak.proto.login.LoginService`

Source: `myteamspeak_login.proto`. 12 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `session` | `Session` | `LoginStatus` | `session` |
| `deleteSession` | `Session` | `LoginStatus` | `session` |
| `login` | `LoginData` | `LoginSession` | `email`, `password`; optional OTP/device fields |
| `requestAuthToken` | `AuthTokenRequest` | `AuthTokenReply` | `email`, `password` |
| `loginWithAuthToken` | `AuthTokenLogin` | `LoginSession` | `auth_token` |
| `loginWithRenewalToken` | `RenewalTokenLogin` | `LoginSession` | `renewal_token`, `auth_token` |
| `getAuthTokenList` | `AuthTokenListRequest` | `AuthTokenListReply` | `email`, `password` |
| `expireAuthTokenAccess` | `AuthTokenExpireRequest` | `AuthTokenExpireReply` | `email`, `password` |
| `deleteAccount` | `AccountStatusInfo` | `AccountStatusReply` | None; `email` is a target identifier |
| `suspendAccount` | `AccountStatusInfo` | `AccountStatusReply` | None; `email` is a target identifier |
| `reactivateAccount` | `AccountStatusInfo` | `AccountStatusReply` | None; `email` is a target identifier |
| `listSuspensionReasonForUser` | `AccountEmail` | `SuspensionReasonForUserReply` | None; `email` is a target identifier |

### `com.teamspeak.myteamspeak.proto.messengerconnector.MessengerConnectorClientService`

Source: `myteamspeak_messenger_connector_client.proto`. 1 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `requestCreateMessengerAccount` | `RequestCreateMessengerAccountRequest` | `RequestCreateMessengerAccountReply` | `session` |

### `com.teamspeak.myteamspeak.proto.synchronization.SynchronizationService`

Source: `myteamspeak_synchronization.proto`. 2 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `requestServerItems` | `Sync_Request_ItemClasses` | `Sync_Reply_ItemClasses` | `session` |
| `synchronizeItems` | `Sync_Request_ItemClasses` | `Sync_Reply_ItemClasses` | `session` |

### `com.teamspeak.myteamspeak.proto.tschat.ChatRequests`

Source: `myteamspeak_tschat.proto`. 16 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `createAccount` | `AuthenticatedUser` | `CreateAccountResponse` | `session`; `matrix_id` |
| `moveHome` | `MoveRequest` | `ErrorHandling` | `user.session`; Matrix ID in `user` |
| `getContactList` | `AuthenticatedUser` | `ContactList` | `session`; `matrix_id` |
| `updateContact` | `ContactRequest` | `ErrorHandling` | `user.session`; Matrix ID in `user` |
| `updateContactList` | `ContactRequestList` | `ErrorHandling` | `user.session`; Matrix ID in `user` |
| `removeContact` | `ContactRequest` | `ErrorHandling` | `user.session`; Matrix ID in `user` |
| `setPrimaryIdentifier` | `IdentifierRequest` | `TschatIdentifierList` | `user.session`; Matrix ID in `user` |
| `addIdentifier` | `IdentifierRequest` | `TschatIdentifierList` | `user.session`; Matrix ID in `user` |
| `removeIdentifier` | `IdentifierRequest` | `TschatIdentifierList` | `user.session`; Matrix ID in `user` |
| `getActiveIdentifierList` | `AuthenticatedUser` | `TschatIdentifierList` | `session`; `matrix_id` |
| `getAllowedIdentifierList` | `AuthenticatedUser` | `TschatIdentifierList` | `session`; `matrix_id` |
| `getGroupSessionData` | `GroupSessionRequest` | `GroupSessionResponse` | `user.session`; Matrix ID in `user` |
| `uploadGroupSessionData` | `GroupSessionRequest` | `ErrorHandling` | `user.session`; Matrix ID in `user` |
| `clearAllGroupSessions` | `AuthenticatedUser` | `ErrorHandling` | `session`; `matrix_id` |
| `clearGroupSessions` | `GroupSessionRequest` | `ErrorHandling` | `user.session`; Matrix ID in `user` |
| `requestSignedAllowedIdentifierList` | `AuthenticatedUser` | `SignedAllowedIdentifier` | `session`; `matrix_id` |

### `com.teamspeak.myteamspeak.proto.user.UserAccountService`

Source: `myteamspeak_user.proto`. 30 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `addUser` | `UserData` | `ReturnCode` | `session`; `login` / `login_old` credentials |
| `updateUser` | `UserData` | `ReturnCode` | `session`; `login` / `login_old` credentials |
| `getBackupKey` | `UserLogin` | `BackupKeyData` | `email`, `password` |
| `getBadges` | `Session` | `UserBadgesList` | `session` |
| `redeemBadgeCode` | `BadgeCode` | `RedeemBadgeCodeResponse` | `session` |
| `setNewUserPublicKey` | `NewUserPublicKeyData` | `NewUserPublicKeyDataResponse` | `session.session` |
| `requestContactsAvatar` | `RequestContactsAvatarInfoRequest` | `RequestContactsAvatarInfoResponse` | `session` |
| `getSignedBadges` | `Session` | `UserBadgesSignedResponse` | `session` |
| `requestVoiceServer` | `VoiceServerRequest` | `VoiceServerResponse` | `session` |
| `requestVoiceServerLocation` | `SpawnVoiceServerLocationRequest` | `SpawnVoiceServerLocationResponse` | `session` |
| `deleteSpawnedVoiceServer` | `VoiceServerRequest` | `ReturnCode` | `session` |
| `listSpawnedVoiceServer` | `Session` | `SpawnedVoiceServerListResponse` | `session` |
| `startVoiceServer` | `VoiceServerRequest` | `VoiceServerResponse` | `session` |
| `stopVoiceServer` | `VoiceServerRequest` | `VoiceServerResponse` | `session` |
| `readVoiceServerStatus` | `VoiceServerRequest` | `VoiceServerResponse` | `session` |
| `requestAvatarSignedUrl` | `AvatarSignedUrlRequest` | `AvatarSignedUrlResponse` | `session` |
| `requestUploadAvatar` | `RequestUploadAvatarRequest` | `RequestUploadAvatarResponse` | `session` |
| `requestDeleteAvatar` | `RequestDeleteAvatarInfoRequest` | `ReturnCode` | `session` |
| `changeEmail` | `EmailChange` | `ReturnCode` | `session`; `validationKey`, `currentPw`, `newPw` |
| `resetAccount` | `ResetAccountRequest` | `ReturnCode` | `session`, `myts_id.session.session` |
| `updateUserDescription` | `UpdateUserDescriptionRequest` | `ReturnCode` | `session` |
| `requestUploadFile` | `UploadFileRequest` | `FileInfo` | `session` |
| `requestDownloadFile` | `DownloadFileRequest` | `FileInfo` | `session` |
| `generateOtpSetupSecret` | `TwoFactorAuthTypeRequest` | `OtpSecretResponse` | `session`; `user_login` credentials |
| `updateTwoFactorAuthType` | `UpdateTwoFactorAuthTypeRequest` | `ReturnCode` | `session` |
| `confirmTwoFactorAuthTypeOtp` | `TwoFactorAuthTypeOtpConfirmationRequest` | `ReturnCode` | `session`; `user_login` credentials; `otp` |
| `removeTwoFactorAuthType` | `TwoFactorAuthTypeRequest` | `ReturnCode` | `session`; `user_login` credentials |
| `getTwoFactorAuthType` | `Session` | `TwoFactorAuthTypeResponse` | `session` |
| `registerFirebasePush` | `RegisterFirebasePush` | `ReturnCode` | `session` |
| `getAccountData` | `AccountDataRequest` | `UserAccountData` | `session` |

### `com.teamspeak.myteamspeak.proto.management.user.UserManagementService`

Source: `myteamspeak_user_management.proto`. 39 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `listUser` | `SearchLimit` | `UserLightDataList` | None declared |
| `getUserInfo` | `Request` | `UserLightData` | `session` |
| `getKey` | `Request` | `KeyData` | `session` |
| `deleteUser` | `Request` | `UserManagementReturnCode` | `session` |
| `activateUser` | `Activation` | `UserManagementReturnCode` | `email`, activation `key` |
| `requestNewActivation` | `Activation` | `UserManagementReturnCode` | `email`, activation `key` |
| `setAddonDevStatus` | `AddonDevRequest` | `AddonDevReturnCode` | None declared |
| `requestPasswordReset` | `PasswordResetRequest` | `UserManagementReturnCode` | `email`, reset `key`, new `password` |
| `resetPassword` | `PasswordResetRequest` | `UserManagementReturnCode` | `email`, reset `key`, new `password` |
| `generateBadgeCodes` | `BadgeCodesRequest` | `BadgeCodes` | `session` |
| `changeEmail` | `EmailChange` | `UserManagementReturnCode` | `session`; `validationKey`, `currentPw`, `newPw` |
| `changeEmail2` | `EmailChange` | `UserManagementReturnCode` | `session`; `validationKey`, `currentPw`, `newPw` |
| `changeEmail3` | `EmailChange` | `UserManagementReturnCode` | `session`; `validationKey`, `currentPw`, `newPw` |
| `assignBadge` | `AssignBadgeRequest` | `UserManagementReturnCode` | `session` |
| `getUserData` | `Request` | `UserFullData` | `session` |
| `forceRename` | `UserRenameRequest` | `UserManagementReturnCode` | `session` |
| `addBadge` | `AddBadgeRequest` | `AddBadgeReturnCode` | `session` |
| `listBadges` | `Session` | `ListBadgesReturnCode` | `session` |
| `getBadgeStatistics` | `BadgeStatisticsRequest` | `BadgeStatisticsReturnCode` | `session` |
| `deleteBadge` | `Request` | `DeleteBadgeReturnCode` | `session` |
| `updateBadge` | `UpdateBadgeRequest` | `AddBadgeReturnCode` | `addBadgeRequest.session` |
| `updateBadgeCode` | `BadgeCodesRequest` | `BadgeCodeReturnCode` | `session` |
| `deleteBadgeCode` | `BadgeCodesRequest` | `UserManagementReturnCode` | `session` |
| `listBadgeCodes` | `BadgeCodesRequest` | `ListBadgeCodesResponse` | `session` |
| `addOAuthClient` | `OAuthClientRequest` | `OAuthClientReturnCode` | `session` |
| `updateOAuthClient` | `OAuthClientRequest` | `OAuthClientReturnCode` | `session` |
| `deleteOAuthClient` | `OAuthClientRequest` | `UserManagementReturnCode` | `session` |
| `listOAuthClients` | `Request` | `ListOAuthClientsReturnCode` | `session` |
| `searchUser` | `SearchTerm` | `UserLightDataList` | `session` |
| `addPermission` | `PermissionRequest` | `UserManagementReturnCode` | `session` |
| `removePermission` | `PermissionRequest` | `UserManagementReturnCode` | `session` |
| `removeBadgeFromUser` | `AssignBadgeRequest` | `UserManagementReturnCode` | `session` |
| `createCommunity` | `TSCommunityRequest` | `TSCommunityList` | `session` |
| `listCommunities` | `TSCommunityListRequest` | `TSCommunityList` | `request.session` |
| `updateCommunity` | `TSCommunityRequest` | `TSCommunityList` | `session` |
| `setRenameAllowance` | `RenameAllowanceRequest` | `UserManagementReturnCode` | `session` |
| `deleteTSChatAccount` | `Request` | `UserManagementReturnCode` | `session` |
| `getUserStatistics` | `UserStatisticsRequest` | `UserStatisticsResponse` | `session` |
| `getAllUserStatistics` | `UserStatisticsRequest` | `AllUserStatisticsResponse` | `session` |

### `com.teamspeak.push.proto.PushService`

Source: `teamspeak_push_grpc.proto`. 1 methods.

| Method | Request | Response | Body fields |
| --- | --- | --- | --- |
| `longPull` | `google.protobuf.Empty` | `stream Message` | None declared |

## Application enum catalog

All enum values, including nested enum values and sparse numeric assignments, are recorded in the JSON inventory. Counts below count explicitly declared variants, not integer ranges. Enum values whose names sound successful must be interpreted in their owning response domain.

| Fully qualified enum | Variants |
| --- | ---: |
| `com.teamspeak.sync.proto.Bookmark_Data.SubscriptionMode` | 3 |
| `com.teamspeak.sync.proto.Whisper_list_data.WhisperListType` | 3 |
| `com.teamspeak.sync.proto.Whisper_list_data.WhisperEntry.WhisperEntryType` | 3 |
| `com.teamspeak.sync.proto.HotkeyProfile_data.Hotkey.OS` | 3 |
| `com.teamspeak.sync.proto.Contact.NotificationPreference` | 4 |
| `com.teamspeak.sync.proto.Item_Data.Manipulation_Flag` | 4 |
| `com.teamspeak.blacklist.BlacklistInfoResponse.Result` | 4 |
| `com.teamspeak.myteamspeak.proto.addon.ApiType` | 7 |
| `com.teamspeak.myteamspeak.proto.addon.ReturnCode` | 6 |
| `com.teamspeak.myteamspeak.proto.addon.Platform` | 7 |
| `com.teamspeak.myteamspeak.proto.addon.Status` | 7 |
| `com.teamspeak.myteamspeak.proto.AvatarError` | 4 |
| `com.teamspeak.myteamspeak.proto.AvatarState` | 5 |
| `com.teamspeak.myteamspeak.proto.AddonDevStatus` | 7 |
| `com.teamspeak.myteamspeak.proto.LoginOrigin` | 2 |
| `com.teamspeak.myteamspeak.proto.ErrorCommon` | 18 |
| `com.teamspeak.myteamspeak.proto.integration.SpecifierType` | 5 |
| `com.teamspeak.myteamspeak.proto.integration.IntegrationType` | 3 |
| `com.teamspeak.myteamspeak.proto.integration.IntegrationBindingStatus` | 4 |
| `com.teamspeak.myteamspeak.proto.integration.IntegrationError` | 12 |
| `com.teamspeak.myteamspeak.proto.integration.ResponseType` | 5 |
| `com.teamspeak.myteamspeak.proto.messengerconnector.MessengerConnectorReturnCodesClient` | 4 |
| `com.teamspeak.myteamspeak.proto.push.SimpleNotificationType` | 4 |
| `com.teamspeak.myteamspeak.proto.tschat.ChatRequestReturnCode` | 7 |
| `com.teamspeak.myteamspeak.proto.user.UserAccountDataSelector` | 15 |
| `com.teamspeak.myteamspeak.proto.user.AuthType` | 2 |
| `com.teamspeak.myteamspeak.proto.user.ErrorReturnCode` | 34 |
| `com.teamspeak.myteamspeak.proto.management.user.TSCommunityState` | 4 |
| `com.teamspeak.myteamspeak.proto.management.user.UserSearchCriterionSelector` | 8 |
| `com.teamspeak.myteamspeak.proto.management.user.UserManagementErrorReturnCode` | 41 |
| `com.teamspeak.myteamspeak.proto.synchronization.SyncStatus` | 10 |
| `com.teamspeak.myteamspeak.proto.synchronization.Item_Class` | 9 |
| `com.teamspeak.myteamspeak.proto.synchronization.Sync_Version` | 2 |

## All archive packages

These are source-level counts across the entire archive. “App” marks packages declared by root application files. Infrastructure bindings may use compiler-vendored Google definitions in place of malformed bundled sources, so generated descriptor counts can differ from these archive counts.

| Package | Scope | Files | Messages | Enums | Services | RPCs |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `com.teamspeak.account.proto` | App | 1 | 8 | 0 | 0 | 0 |
| `com.teamspeak.blacklist` | App | 1 | 2 | 1 | 0 | 0 |
| `com.teamspeak.myteamspeak.proto` | App | 2 | 23 | 5 | 0 | 0 |
| `com.teamspeak.myteamspeak.proto.addon` | App | 1 | 4 | 4 | 1 | 1 |
| `com.teamspeak.myteamspeak.proto.cloud` | App | 1 | 2 | 0 | 0 | 0 |
| `com.teamspeak.myteamspeak.proto.integration` | App | 3 | 21 | 5 | 1 | 4 |
| `com.teamspeak.myteamspeak.proto.login` | App | 1 | 12 | 0 | 1 | 12 |
| `com.teamspeak.myteamspeak.proto.management.user` | App | 1 | 43 | 3 | 1 | 39 |
| `com.teamspeak.myteamspeak.proto.messengerconnector` | App | 1 | 5 | 1 | 1 | 1 |
| `com.teamspeak.myteamspeak.proto.namedserver` | App | 1 | 4 | 0 | 0 | 0 |
| `com.teamspeak.myteamspeak.proto.push` | App | 1 | 6 | 1 | 0 | 0 |
| `com.teamspeak.myteamspeak.proto.synchronization` | App | 2 | 4 | 3 | 1 | 2 |
| `com.teamspeak.myteamspeak.proto.tschat` | App | 1 | 19 | 1 | 1 | 16 |
| `com.teamspeak.myteamspeak.proto.user` | App | 1 | 55 | 3 | 1 | 30 |
| `com.teamspeak.proto.client_cache` | App | 1 | 1 | 0 | 0 | 0 |
| `com.teamspeak.proto.revocation` | App | 1 | 1 | 0 | 0 | 0 |
| `com.teamspeak.push.proto` | App | 1 | 1 | 0 | 1 | 1 |
| `com.teamspeak.revocation_list` | App | 1 | 1 | 0 | 0 | 0 |
| `com.teamspeak.sync.proto` | App | 1 | 16 | 6 | 0 | 0 |
| `envoy.annotations` | Infrastructure | 1 | 0 | 0 | 0 | 0 |
| `envoy.config.accesslog.v3` | Infrastructure | 1 | 16 | 2 | 0 | 0 |
| `envoy.config.cluster.v3` | Infrastructure | 4 | 29 | 9 | 0 | 0 |
| `envoy.config.core.v3` | Infrastructure | 16 | 97 | 13 | 0 | 0 |
| `envoy.config.endpoint.v3` | Infrastructure | 3 | 16 | 0 | 0 | 0 |
| `envoy.config.listener.v3` | Infrastructure | 5 | 20 | 2 | 0 | 0 |
| `envoy.config.rbac.v3` | Infrastructure | 1 | 11 | 3 | 0 | 0 |
| `envoy.config.route.v3` | Infrastructure | 3 | 62 | 6 | 0 | 0 |
| `envoy.config.trace.v3` | Infrastructure | 1 | 2 | 0 | 0 | 0 |
| `envoy.data.accesslog.v3` | Infrastructure | 1 | 11 | 4 | 0 | 0 |
| `envoy.extensions.clusters.aggregate.v3` | Infrastructure | 1 | 1 | 0 | 0 | 0 |
| `envoy.extensions.filters.common.fault.v3` | Infrastructure | 1 | 5 | 1 | 0 | 0 |
| `envoy.extensions.filters.http.fault.v3` | Infrastructure | 1 | 3 | 0 | 0 | 0 |
| `envoy.extensions.filters.http.gcp_authn.v3` | Infrastructure | 1 | 4 | 0 | 0 | 0 |
| `envoy.extensions.filters.http.rbac.v3` | Infrastructure | 1 | 2 | 0 | 0 | 0 |
| `envoy.extensions.filters.http.router.v3` | Infrastructure | 1 | 2 | 0 | 0 | 0 |
| `envoy.extensions.filters.http.stateful_session.v3` | Infrastructure | 1 | 2 | 0 | 0 | 0 |
| `envoy.extensions.filters.network.http_connection_manager.v3` | Infrastructure | 1 | 21 | 5 | 0 | 0 |
| `envoy.extensions.http.stateful_session.cookie.v3` | Infrastructure | 1 | 1 | 0 | 0 | 0 |
| `envoy.extensions.transport_sockets.http_11_proxy.v3` | Infrastructure | 1 | 1 | 0 | 0 | 0 |
| `envoy.extensions.transport_sockets.tls.v3` | Infrastructure | 3 | 18 | 4 | 0 | 0 |
| `envoy.extensions.upstreams.http.v3` | Infrastructure | 1 | 4 | 0 | 0 | 0 |
| `envoy.service.discovery.v3` | Infrastructure | 1 | 13 | 0 | 0 | 0 |
| `envoy.service.load_stats.v3` | Infrastructure | 1 | 2 | 0 | 1 | 1 |
| `envoy.type.http.v3` | Infrastructure | 2 | 5 | 0 | 0 | 0 |
| `envoy.type.matcher.v3` | Infrastructure | 8 | 15 | 0 | 0 | 0 |
| `envoy.type.metadata.v3` | Infrastructure | 1 | 7 | 0 | 0 | 0 |
| `envoy.type.tracing.v3` | Infrastructure | 1 | 5 | 0 | 0 | 0 |
| `envoy.type.v3` | Infrastructure | 4 | 6 | 2 | 0 | 0 |
| `google.api.expr.v1alpha1` | Infrastructure | 2 | 25 | 3 | 0 | 0 |
| `google.protobuf` | Infrastructure | 9 | 56 | 24 | 0 | 0 |
| `google.rpc` | Infrastructure | 1 | 1 | 0 | 0 | 0 |
| `grpc.channelz.v2` | Infrastructure | 2 | 16 | 1 | 0 | 0 |
| `grpc.lookup.v1` | Infrastructure | 1 | 7 | 0 | 0 | 0 |
| `pb` | Infrastructure | 1 | 1 | 1 | 0 | 0 |
| `udpa.annotations` | Infrastructure | 5 | 6 | 1 | 0 | 0 |
| `validate` | Infrastructure | 1 | 23 | 1 | 0 | 0 |
| `xds.annotations.v3` | Infrastructure | 1 | 4 | 1 | 0 | 0 |
| `xds.core.v3` | Infrastructure | 6 | 8 | 1 | 0 | 0 |
| `xds.type.matcher.v3` | Infrastructure | 3 | 13 | 0 | 0 | 0 |

### Ancillary service

`envoy.service.load_stats.v3.LoadReportingService/StreamLoadStats` is bidirectional streaming from `LoadStatsRequest` to `LoadStatsResponse`. It is an infrastructure protocol separate from the 106 application methods.

## What requires server-side knowledge

A running integration still requires endpoint(s), authorization metadata if any, deployment TLS material, credential conventions, and any encrypted payload/signature conventions. User-management pagination includes both cursor-like `SearchLimit.last_uuid` and offset/limit requests; neither establishes ordering guarantees or termination rules for automatic pagination. No API version compatibility range, rate-limit policy, server time unit, or guaranteed default behavior is documented. These omissions should stay explicit in a full-featured client rather than being replaced by guessed protocol behavior.
