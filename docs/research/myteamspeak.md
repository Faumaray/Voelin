# myTeamSpeak

## Conclusion: out of scope

myTeamSpeak is TeamSpeak's account service: it holds the user's identities,
bookmarks and settings server-side so the official clients can synchronise
them. Voelin does **not** sign in to it.

There is no public API for it. The endpoints the official client uses are not
documented for third parties, and TeamSpeak's terms of service do not permit
using non-public ones. So there is nothing to implement here that could be
maintained, and nothing that would be allowed. This file exists so the question
is not re-researched.

What the user gets instead: sign in with the official client once, so that it
writes the synchronised identities into its local `settings.db`, then
`voelinctl identity import` reads them from there. See
[../identity.md](../identity.md).

## The local record, structurally

The TeamSpeak 6 client keeps its account state in its `settings.db` in the
table `AccountData`, shape `(timestamp integer unsigned, key varchar unique,
value varchar)`, with two rows: `Account` and `Checksum`. `Checksum` is 20
bytes — `SHA1` of the other rows' values, the same rule as `ProtobufItems`
(see [../identity.md](../identity.md)).

`Account` is a protobuf message, about 1.7 kB in the install examined. Its
shape, field numbers and lengths only:

| Field | Wire type | Length | Looks like |
| --- | --- | --- | --- |
| 1 | bytes | 18 | printable ASCII |
| 2 | bytes | 64 | printable ASCII, base64 alphabet |
| 3 | bytes | 36 | a UUID |
| 4 | bytes | 8 | printable ASCII, base64 alphabet |
| 5 | bytes | 32 | binary |
| 10 | bytes | 36 | a UUID |
| 50 | message | 38 | one nested field: a UUID |
| 51 | message | 2–5 | repeated, nine times; one or two small fields each |
| 52 | bytes | 61 | binary |
| 54 | message | 375 | 32-byte and 33-byte binary fields, a varint, a nested 112-byte blob, a 176-byte blob |
| 56 | message | 114 | one nested field: a 112-byte blob |
| 57 | message | 178 | a nested 98-character ASCII string, a varint, a 64-byte blob |
| 58 | bytes | 386 | printable ASCII |
| 59 | message | 6 | one small field |
| 61 | message | 241 | a 64-byte and a 112-byte blob |

The 386-character printable field and the 64-byte base64-alphabet field are the
shape of a bearer token and an identifier; **they were not decoded, parsed or
used, and no request was made to any TeamSpeak service.**

No field in `Account` parses as an ASN.1 `SEQUENCE`, so it carries **no
identity key pair** in the tomcrypt DER form the identities use. Importing
identities therefore never needs to touch this row, and Voelin's importer does
not read it. The 112-byte blobs are the right size for a raw key or signature
but are not DER; what they are was not investigated further, because nothing
Voelin does needs them.

`avatar_cache_mytsid` (one row, `contacts`) and `RevocationList` (one row,
`Cache`) are caches the account service fills. Neither is read either.
