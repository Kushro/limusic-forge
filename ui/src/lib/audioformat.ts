// The quality readout under the volume slider and in the mini player: a tier badge plus
// "FLAC • 24-bit • 96 kHz • 2304 kbps". Pure, so audioformat.check.ts can run it under plain node.

/** Same shape as `api.AudioFormat`, restated so this module imports nothing from Tauri. */
export interface AudioFormatFacts {
	codec: string | null;
	sampleRate: number | null;
	bitDepth: number | null;
	bitrateKbps: number | null;
	lossless: boolean;
}

/**
 * Hi-res is lossless past CD quality: more than 16 bits or more than 48 kHz. That's the line most
 * players and stores draw (the stricter 24/96 one would call a 24/48 studio master "lossless" next
 * to a 16/44.1 rip). Everything YouTube serves is lossy.
 */
export type QualityTier = 'hires' | 'lossless' | 'lossy';

export function qualityTier(f: AudioFormatFacts): QualityTier {
	if (!f.lossless) return 'lossy';
	return (f.bitDepth ?? 0) > 16 || (f.sampleRate ?? 0) > 48000 ? 'hires' : 'lossless';
}

/** ffmpeg names its decoders, not its formats: `mp3float` is MP3, `pcm_s24le` is PCM. */
export function codecLabel(codec: string): string {
	const c = codec.toLowerCase();
	if (c.startsWith('pcm_')) return 'PCM';
	const named: Record<string, string> = {
		mp3float: 'MP3',
		mp3: 'MP3',
		aac_fixed: 'AAC',
		aac: 'AAC',
		aac_latm: 'AAC',
		opus: 'OPUS',
		libopus: 'OPUS',
		vorbis: 'VORBIS',
		libvorbis: 'VORBIS',
		flac: 'FLAC',
		alac: 'ALAC',
		wavpack: 'WAVPACK',
		wmalossless: 'WMA LOSSLESS',
		eac3: 'E-AC-3',
		ac3: 'AC-3'
	};
	return named[c] ?? c.toUpperCase();
}

/** 44100 → "44.1 kHz", 48000 → "48 kHz", 176400 → "176.4 kHz". */
export function formatSampleRate(hz: number): string {
	const khz = Math.round(hz / 100) / 10;
	return `${Number.isInteger(khz) ? khz : khz.toFixed(1)} kHz`;
}

/** The readout's text after the badge, or `[]` when nothing is known yet. */
export function formatParts(f: AudioFormatFacts): string[] {
	if (!f.codec) return [];
	const parts = [codecLabel(f.codec)];
	if (f.lossless && f.bitDepth) parts.push(`${f.bitDepth}-bit`);
	if (f.sampleRate) parts.push(formatSampleRate(f.sampleRate));
	if (f.bitrateKbps) parts.push(`${f.bitrateKbps} kbps`);
	return parts;
}

export const SEPARATOR = ' • ';
