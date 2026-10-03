//! DTLS for our peers: str0m's dimpl DTLS (aws-lc-rs), configured with our
//! order of SRTP protection profiles.
//!
//! str0m takes its DTLS implementation from a [`CryptoProvider`], whose DTLS
//! part builds a dimpl `Config` we cannot change. [`crypto_provider`]
//! returns str0m's aws-lc-rs provider with that part replaced by
//! [`VoelinDtlsProvider`], which passes our profile order to dimpl (vendored
//! with a patch for it, see `third_party/dimpl/VOELIN-PATCH.md`).
//!
//! Its DTLS instances also record the profile the handshake negotiated,
//! which str0m does not expose: [`build_rtc`] returns it as a
//! [`NegotiatedProfile`].

use std::cell::RefCell;
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use str0m::crypto::dtls::{
	DtlsCert, DtlsImplError, DtlsInstance, DtlsOutput, DtlsProvider, DtlsVersion, ProtocolVersion,
};
use str0m::crypto::{CryptoError, CryptoProvider};
use str0m::{Rtc, RtcConfig};

/// An SRTP protection profile (RFC 5764, RFC 7714).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SrtpProfile {
	/// `AES_CM_128_HMAC_SHA1_80`: what official TeamSpeak clients use.
	Aes128CmSha1_80,
	/// `AEAD_AES_128_GCM`
	AeadAes128Gcm,
	/// `AEAD_AES_256_GCM`
	AeadAes256Gcm,
}

impl SrtpProfile {
	/// All profiles, in our default order of preference.
	pub const ALL: [Self; 3] = [Self::Aes128CmSha1_80, Self::AeadAes128Gcm, Self::AeadAes256Gcm];

	/// Our default order: AES_CM_128_HMAC_SHA1_80 first, as between official
	/// clients, then the GCM profiles for peers that do not offer it.
	pub const DEFAULT_ORDER: [Self; 3] = Self::ALL;

	/// An AEAD profile (AES-GCM, RFC 7714): an SRTP library either has both
	/// or neither.
	pub fn is_aead(self) -> bool {
		matches!(self, Self::AeadAes128Gcm | Self::AeadAes256Gcm)
	}

	/// The profile's name as in RFC 5764/7714 and WebRTC statistics
	/// (`srtpCipher`).
	pub fn name(self) -> &'static str {
		match self {
			Self::Aes128CmSha1_80 => "AES_CM_128_HMAC_SHA1_80",
			Self::AeadAes128Gcm => "AEAD_AES_128_GCM",
			Self::AeadAes256Gcm => "AEAD_AES_256_GCM",
		}
	}

	/// From [`name`](Self::name), or the `SRTP_...` names of RFC 5764's
	/// registry and dimpl (`SRTP_AES128_CM_HMAC_SHA1_80`,
	/// `SRTP_AES128_CM_SHA1_80`, `SRTP_AEAD_AES_128_GCM`, ...); ASCII case
	/// is ignored.
	pub fn from_name(name: &str) -> Option<Self> {
		let name = name.trim().to_ascii_uppercase();
		let name = name.strip_prefix("SRTP_").unwrap_or(&name);
		match name {
			"AES_CM_128_HMAC_SHA1_80" | "AES128_CM_HMAC_SHA1_80" | "AES128_CM_SHA1_80" => {
				Some(Self::Aes128CmSha1_80)
			}
			"AEAD_AES_128_GCM" => Some(Self::AeadAes128Gcm),
			"AEAD_AES_256_GCM" => Some(Self::AeadAes256Gcm),
			_ => None,
		}
	}

	fn to_dimpl(self) -> dimpl::SrtpProfile {
		match self {
			Self::Aes128CmSha1_80 => dimpl::SrtpProfile::AES128_CM_SHA1_80,
			Self::AeadAes128Gcm => dimpl::SrtpProfile::AEAD_AES_128_GCM,
			Self::AeadAes256Gcm => dimpl::SrtpProfile::AEAD_AES_256_GCM,
		}
	}

	fn from_dimpl(profile: dimpl::SrtpProfile) -> Option<Self> {
		Self::ALL.into_iter().find(|p| p.to_dimpl() == profile)
	}

	fn code(self) -> u8 {
		match self {
			Self::Aes128CmSha1_80 => 1,
			Self::AeadAes128Gcm => 2,
			Self::AeadAes256Gcm => 3,
		}
	}

	fn from_code(code: u8) -> Option<Self> {
		Self::ALL.into_iter().find(|p| p.code() == code)
	}
}

impl fmt::Display for SrtpProfile {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.name())
	}
}

/// The SRTP profile a peer connection's DTLS handshake negotiated; unset
/// until the handshake is done.
#[derive(Clone, Debug, Default)]
pub struct NegotiatedProfile(Arc<AtomicU8>);

impl NegotiatedProfile {
	pub fn get(&self) -> Option<SrtpProfile> {
		SrtpProfile::from_code(self.0.load(Ordering::Relaxed))
	}

	fn set(&self, profile: SrtpProfile) {
		self.0.store(profile.code(), Ordering::Relaxed);
	}
}

/// str0m's DTLS (dimpl with aws-lc-rs) with our SRTP profile order.
#[derive(Debug)]
pub struct VoelinDtlsProvider {
	profiles: Vec<dimpl::SrtpProfile>,
}

impl VoelinDtlsProvider {
	fn new(order: &[SrtpProfile]) -> Self {
		Self { profiles: order.iter().map(|p| p.to_dimpl()).collect() }
	}
}

thread_local! {
	/// Where the next DTLS instance built on this thread records its
	/// profile; set by [`build_rtc`] around `RtcConfig::build`, which
	/// creates the instance synchronously.
	static NEXT_PROFILE: RefCell<Option<NegotiatedProfile>> = const { RefCell::new(None) };
}

impl DtlsProvider for VoelinDtlsProvider {
	fn generate_certificate(&self) -> Option<DtlsCert> {
		dimpl::certificate::generate_self_signed_certificate().ok()
	}

	fn new_dtls(
		&self,
		cert: &DtlsCert,
		now: Instant,
		dtls_version: DtlsVersion,
		mtu: Option<usize>,
	) -> Result<Box<dyn DtlsInstance>, CryptoError> {
		// As str0m's own provider: ICE already verified the peer's address,
		// so server cookies are redundant.
		let mut builder =
			dimpl::Config::builder().use_server_cookie(false).srtp_profiles(&self.profiles);
		if let Some(mtu) = mtu {
			builder = builder.mtu(mtu);
		}
		let config = Arc::new(
			builder.build().map_err(|e| CryptoError::Other(format!("dimpl config: {e}")))?,
		);
		let cert = cert.clone();
		let dtls = match dtls_version {
			DtlsVersion::Dtls12 => dimpl::Dtls::new_12(config, cert, now),
			DtlsVersion::Dtls13 => dimpl::Dtls::new_13(config, cert, now),
			DtlsVersion::Auto => dimpl::Dtls::new_auto(config, cert, now),
			other => return Err(CryptoError::Other(format!("unsupported DTLS version {other}"))),
		};
		let profile = NEXT_PROFILE.with(|next| next.borrow_mut().take()).unwrap_or_default();
		Ok(Box::new(Instance { dtls, profile }))
	}
}

struct Instance {
	dtls: dimpl::Dtls,
	profile: NegotiatedProfile,
}

impl fmt::Debug for Instance {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("VoelinDtlsInstance").field("profile", &self.profile.get()).finish()
	}
}

impl DtlsInstance for Instance {
	fn set_active(&mut self, active: bool) {
		self.dtls.set_active(active);
	}

	fn handle_packet(&mut self, packet: &[u8]) -> Result<(), DtlsImplError> {
		self.dtls.handle_packet(packet)
	}

	fn poll_output<'a>(&mut self, buf: &'a mut [u8]) -> DtlsOutput<'a> {
		let output = self.dtls.poll_output(buf);
		if let DtlsOutput::KeyingMaterial(_, profile) = &output
			&& let Some(profile) = SrtpProfile::from_dimpl(*profile)
		{
			self.profile.set(profile);
		}
		output
	}

	fn handle_timeout(&mut self, now: Instant) -> Result<(), DtlsImplError> {
		self.dtls.handle_timeout(now)
	}

	fn send_application_data(&mut self, data: &[u8]) -> Result<(), DtlsImplError> {
		self.dtls.send_application_data(data)
	}

	fn is_active(&self) -> bool {
		self.dtls.is_active()
	}

	fn protocol_version(&self) -> Option<ProtocolVersion> {
		self.dtls.protocol_version()
	}

	fn is_closing(&self) -> bool {
		self.dtls.is_closing()
	}

	fn is_closed(&self) -> bool {
		self.dtls.is_closed()
	}

	fn close(&mut self) -> Result<(), DtlsImplError> {
		self.dtls.close()
	}
}

/// str0m's aws-lc-rs crypto with [`VoelinDtlsProvider`] for `order` (our
/// default order if empty). One provider per distinct order is created and
/// kept for the process (str0m needs `'static` providers); there are at most
/// 15 distinct orders of the three profiles.
pub fn crypto_provider(order: &[SrtpProfile]) -> Arc<CryptoProvider> {
	static PROVIDERS: Mutex<Vec<(Vec<SrtpProfile>, Arc<CryptoProvider>)>> = Mutex::new(Vec::new());
	let mut key: Vec<SrtpProfile> = Vec::with_capacity(SrtpProfile::ALL.len());
	for p in order {
		if !key.contains(p) {
			key.push(*p);
		}
	}
	if key.is_empty() {
		key.extend(SrtpProfile::DEFAULT_ORDER);
	}
	let mut providers = PROVIDERS.lock().unwrap_or_else(PoisonError::into_inner);
	if let Some((_, provider)) = providers.iter().find(|(k, _)| *k == key) {
		return provider.clone();
	}
	let dtls: &'static VoelinDtlsProvider = Box::leak(Box::new(VoelinDtlsProvider::new(&key)));
	let provider =
		Arc::new(CryptoProvider { dtls_provider: dtls, ..str0m::crypto::from_feature_flags() });
	providers.push((key, provider.clone()));
	provider
}

/// Build an RTC with our DTLS for `order`; also returns where its DTLS
/// handshake records the negotiated SRTP profile.
pub fn build_rtc(config: RtcConfig, order: &[SrtpProfile]) -> (Rtc, NegotiatedProfile) {
	let profile = NegotiatedProfile::default();
	NEXT_PROFILE.with(|next| *next.borrow_mut() = Some(profile.clone()));
	let rtc = config.set_crypto_provider(crypto_provider(order)).build(Instant::now());
	// Normally taken by `new_dtls` already.
	NEXT_PROFILE.with(|next| next.borrow_mut().take());
	(rtc, profile)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn names() {
		for p in SrtpProfile::ALL {
			assert_eq!(SrtpProfile::from_name(p.name()), Some(p));
			assert_eq!(SrtpProfile::from_dimpl(p.to_dimpl()), Some(p));
			assert_eq!(SrtpProfile::from_code(p.code()), Some(p));
		}
		assert_eq!(
			SrtpProfile::from_name("srtp_aes128_cm_sha1_80"),
			Some(SrtpProfile::Aes128CmSha1_80)
		);
		assert_eq!(
			SrtpProfile::from_name("SRTP_AES128_CM_HMAC_SHA1_80"),
			Some(SrtpProfile::Aes128CmSha1_80)
		);
		assert_eq!(
			SrtpProfile::from_name("SRTP_AEAD_AES_256_GCM"),
			Some(SrtpProfile::AeadAes256Gcm)
		);
		assert_eq!(SrtpProfile::from_name("AES_CM_128_HMAC_SHA1_32"), None);
		assert_eq!(SrtpProfile::Aes128CmSha1_80.to_string(), "AES_CM_128_HMAC_SHA1_80");
	}

	#[test]
	fn providers_are_shared_per_order() {
		let a = crypto_provider(&[SrtpProfile::AeadAes128Gcm, SrtpProfile::AeadAes128Gcm]);
		let b = crypto_provider(&[SrtpProfile::AeadAes128Gcm]);
		assert!(Arc::ptr_eq(&a, &b));
		let default = crypto_provider(&[]);
		assert!(Arc::ptr_eq(&default, &crypto_provider(&SrtpProfile::DEFAULT_ORDER)));
		assert!(!Arc::ptr_eq(&a, &default));
	}
}
