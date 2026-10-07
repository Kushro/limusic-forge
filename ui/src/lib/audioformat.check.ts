// Self-check for the quality readout (`audioformat.ts`). No test runner in `ui/` (see
// color.check.ts) — node 22 runs TypeScript directly:
//
//     node --experimental-strip-types ui/src/lib/audioformat.check.ts
//
// Prints "ok" and exits 0, or throws on the first broken invariant.
import { codecLabel, formatParts, formatSampleRate, qualityTier, SEPARATOR } from './audioformat.ts';
import type { AudioFormatFacts } from './audioformat.ts';

function eq<T>(got: T, want: T, what: string): void {
	if (JSON.stringify(got) !== JSON.stringify(want)) {
		throw new Error(`FAIL ${what}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
	}
}

const f = (o: Partial<AudioFormatFacts>): AudioFormatFacts => ({
	codec: null,
	sampleRate: null,
	bitDepth: null,
	bitrateKbps: null,
	lossless: false,
	...o
});

// What YouTube serves: lossy, no bit depth even if one leaks through.
const opus = f({ codec: 'opus', sampleRate: 48000, bitDepth: 32, bitrateKbps: 160 });
eq(qualityTier(opus), 'lossy', 'opus tier');
eq(formatParts(opus).join(SEPARATOR), 'OPUS • 48 kHz • 160 kbps', 'opus text');

// CD quality is lossless, not hi-res.
const cd = f({ codec: 'flac', sampleRate: 44100, bitDepth: 16, bitrateKbps: 900, lossless: true });
eq(qualityTier(cd), 'lossless', 'cd tier');
eq(formatParts(cd).join(SEPARATOR), 'FLAC • 16-bit • 44.1 kHz • 900 kbps', 'cd text');

// Past CD on either axis is hi-res.
eq(qualityTier(f({ codec: 'flac', sampleRate: 44100, bitDepth: 24, lossless: true })), 'hires', '24/44.1');
eq(qualityTier(f({ codec: 'alac', sampleRate: 96000, bitDepth: 16, lossless: true })), 'hires', '16/96');
eq(qualityTier(f({ codec: 'flac', lossless: true })), 'lossless', 'unknown depth/rate stays lossless');

eq(codecLabel('mp3float'), 'MP3', 'mp3float');
eq(codecLabel('pcm_s24le'), 'PCM', 'pcm');
eq(codecLabel('aac_fixed'), 'AAC', 'aac_fixed');
eq(codecLabel('dts'), 'DTS', 'unknown codec uppercased');

eq(formatSampleRate(44100), '44.1 kHz', '44.1');
eq(formatSampleRate(48000), '48 kHz', '48');
eq(formatSampleRate(176400), '176.4 kHz', '176.4');
eq(formatSampleRate(22050), '22.1 kHz', '22.05 rounds to one decimal');

// Nothing known yet → nothing to show.
eq(formatParts(f({ sampleRate: 44100 })), [], 'no codec');

console.log('ok');
