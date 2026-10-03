// Headless Chromium as the remote WebRTC peer for
// crates/voelin-stream/tests/browser_interop.rs (see README.md here).
//
// Reads one JSON command per line on stdin and writes one JSON reply per line
// on stdout; logs go to stderr. Commands (`op`):
//   answer {sdp}          answer an offer, wait for ICE gathering -> {sdp}
//   offer {codec, trickle} send canvas video + oscillator audio; offer with
//                         `codec` preferred -> {sdp} or {unsupported};
//                         with `trickle` the SDP has no candidates
//   candidates            candidates gathered so far -> {candidates: [{candidate, sdpMid, sdpMLineIndex}]}
//   accept {sdp}          apply the answer to our offer
//   candidate {candidate, sdpMid, sdpMLineIndex}   add a remote candidate
//   stats                 connection state, inbound/outbound RTP counters
//                         (incl. the decoder and decoded frame size),
//                         frames shown by a <video> element, the DTLS/SRTP
//                         parameters of the transport
//   quit
'use strict';

const readline = require('node:readline');
// Playwright, or playwright-core driving a browser it did not download
// (`VOELIN_CHROMIUM`, e.g. a system Chromium that decodes H.264).
let chromium;
try {
	({ chromium } = require('playwright'));
} catch {
	({ chromium } = require('playwright-core'));
}

// Runs in the page.
const PAGE_SCRIPT = `
function gathered(pc, ms) {
	return new Promise((resolve) => {
		if (pc.iceGatheringState === 'complete') return resolve();
		pc.addEventListener('icegatheringstatechange', () => {
			if (pc.iceGatheringState === 'complete') resolve();
		});
		setTimeout(resolve, ms);
	});
}

window.peer = {
	pc: null,
	timers: [],
	shownFrames: 0,
	localCandidates: [],

	reset() {
		if (this.pc) this.pc.close();
		this.timers.forEach(clearInterval);
		this.timers = [];
		this.shownFrames = 0;
		this.localCandidates = [];
		this.pc = new RTCPeerConnection();
		this.pc.addEventListener('icecandidate', (e) => {
			if (e.candidate && e.candidate.candidate) this.localCandidates.push(e.candidate.toJSON());
		});
		this.pc.addEventListener('connectionstatechange', () => {
			console.log('connection ' + this.pc.connectionState);
		});
		return this.pc;
	},

	// Count frames a <video> element actually presents (needs decodable video).
	show(track) {
		const video = document.createElement('video');
		video.muted = true;
		video.autoplay = true;
		video.srcObject = new MediaStream([track]);
		document.body.appendChild(video);
		const count = () => {
			this.shownFrames++;
			video.requestVideoFrameCallback(count);
		};
		video.requestVideoFrameCallback(count);
		video.play().catch((e) => console.log('play: ' + e));
	},

	async answer({ sdp }) {
		const pc = this.reset();
		pc.addEventListener('track', (e) => {
			if (e.track.kind === 'video') this.show(e.track);
		});
		await pc.setRemoteDescription({ type: 'offer', sdp });
		await pc.setLocalDescription(await pc.createAnswer());
		await gathered(pc, 3000);
		return { sdp: pc.localDescription.sdp };
	},

	async offer({ codec, trickle }) {
		const pc = this.reset();
		const canvas = document.createElement('canvas');
		canvas.width = 320;
		canvas.height = 240;
		const ctx = canvas.getContext('2d');
		let n = 0;
		this.timers.push(setInterval(() => {
			n++;
			ctx.fillStyle = 'hsl(' + ((n * 7) % 360) + ',60%,50%)';
			ctx.fillRect(0, 0, 320, 240);
			ctx.fillStyle = '#fff';
			ctx.fillText(String(n), 10 + (n % 200), 20 + (n % 150));
		}, 33));
		const video = canvas.captureStream(30).getVideoTracks()[0];
		const audioCtx = new AudioContext();
		const osc = audioCtx.createOscillator();
		const dest = audioCtx.createMediaStreamDestination();
		osc.connect(dest);
		osc.start();
		const audio = dest.stream.getAudioTracks()[0];
		const stream = new MediaStream([video, audio]);
		const transceiver = pc.addTransceiver(video, { direction: 'sendonly', streams: [stream] });
		pc.addTransceiver(audio, { direction: 'sendonly', streams: [stream] });
		if (codec) {
			const all = RTCRtpSender.getCapabilities('video').codecs;
			const wanted = all.filter((c) => c.mimeType.toLowerCase() === 'video/' + codec.toLowerCase());
			if (wanted.length === 0) return { unsupported: codec };
			transceiver.setCodecPreferences([...wanted, ...all.filter((c) => !wanted.includes(c))]);
		}
		await pc.setLocalDescription(await pc.createOffer());
		if (!trickle) await gathered(pc, 3000);
		const sdp = trickle ? pc.localDescription.sdp.split('\\r\\n').filter((l) => !l.startsWith('a=candidate')).join('\\r\\n') : pc.localDescription.sdp;
		return { sdp };
	},

	async candidates() {
		await gathered(this.pc, 3000);
		return { candidates: this.localCandidates };
	},

	async accept({ sdp }) {
		await this.pc.setRemoteDescription({ type: 'answer', sdp });
		return { ok: true };
	},

	async candidate({ candidate, sdpMid, sdpMLineIndex }) {
		await this.pc.addIceCandidate({ candidate, sdpMid, sdpMLineIndex });
		return { ok: true };
	},

	async stats() {
		const report = await this.pc.getStats();
		const codecs = {};
		const inbound = {};
		const outbound = {};
		let transport = {};
		report.forEach((s) => {
			if (s.type === 'codec') codecs[s.id] = s.mimeType;
			if (s.type === 'inbound-rtp') inbound[s.kind] = s;
			if (s.type === 'outbound-rtp') outbound[s.kind] = s;
			if (s.type === 'transport') transport = s;
		});
		const pick = (s, keys) => {
			const o = { codec: codecs[s.codecId] };
			keys.forEach((k) => (o[k] = s[k]));
			return o;
		};
		const out = { connectionState: this.pc.connectionState, shownFrames: this.shownFrames, inbound: {}, outbound: {} };
		out.transport = {};
		['dtlsState', 'dtlsRole', 'tlsVersion', 'dtlsCipher', 'srtpCipher'].forEach((k) => (out.transport[k] = transport[k]));
		for (const k in inbound) {
			out.inbound[k] = pick(inbound[k], ['packetsReceived', 'bytesReceived', 'packetsLost', 'framesReceived', 'framesDecoded', 'keyFramesDecoded', 'framesDropped', 'frameWidth', 'frameHeight', 'decoderImplementation', 'freezeCount', 'pliCount', 'totalSamplesReceived']);
		}
		for (const k in outbound) {
			out.outbound[k] = pick(outbound[k], ['packetsSent', 'bytesSent', 'framesEncoded', 'keyFramesEncoded', 'pliCount']);
		}
		return out;
	},

	async quit() {
		if (this.pc) this.pc.close();
		return { ok: true };
	},
};
`;

async function main() {
	const browser = await chromium.launch({
		headless: true,
		executablePath: process.env.VOELIN_CHROMIUM || undefined,
		args: [
			// Real host candidates instead of mDNS names (no getUserMedia permission here).
			'--disable-features=WebRtcHideLocalIpsWithMdns',
			'--autoplay-policy=no-user-gesture-required',
		],
	});
	const page = await browser.newPage();
	page.on('console', (m) => process.stderr.write('[page] ' + m.text() + '\n'));
	await page.setContent('<!doctype html><html><body></body></html>');
	await page.addScriptTag({ content: PAGE_SCRIPT });
	process.stdout.write(JSON.stringify({ ready: true, userAgent: await page.evaluate(() => navigator.userAgent) }) + '\n');

	const lines = readline.createInterface({ input: process.stdin });
	for await (const line of lines) {
		if (!line.trim()) continue;
		const cmd = JSON.parse(line);
		let reply;
		try {
			reply = await page.evaluate(([op, arg]) => window.peer[op](arg), [cmd.op, cmd]);
		} catch (e) {
			reply = { error: String(e) };
		}
		process.stdout.write(JSON.stringify(reply) + '\n');
		if (cmd.op === 'quit') break;
	}
	await browser.close();
}

main().catch((e) => {
	process.stderr.write(String(e && e.stack ? e.stack : e) + '\n');
	process.exit(1);
});
