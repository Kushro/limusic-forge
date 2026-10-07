<script lang="ts">
	// The quality readout: a tier badge (HI-RES / LOSSLESS / LOSSY) and "FLAC • 24-bit • 96 kHz •
	// 2304 kbps". Where it sits is the user's call (Settings → Appearance), so this only draws it;
	// the player bar and the mini player each place it.
	import { playback } from '$lib/player.svelte';
	import { formatParts, qualityTier, SEPARATOR } from '$lib/audioformat';
	import { t } from '$lib/i18n.svelte';

	let {
		tone = 'muted',
		badgeOnly = false,
		class: klass = ''
	}: {
		/** `art` is white-on-cover for the mini player; `muted` sits on the card in theme colours. */
		tone?: 'muted' | 'art';
		/** Just the badge, the rest in its tooltip: for a spot with no room for the full line. */
		badgeOnly?: boolean;
		class?: string;
	} = $props();

	const f = $derived(playback.now ? playback.audioFormat : null);
	const parts = $derived(f ? formatParts(f) : []);
	const tier = $derived(f ? qualityTier(f) : 'lossy');
	const label = $derived(t(`player.quality.${tier}`));
	const text = $derived(parts.join(SEPARATOR));

	const BADGE = {
		muted: {
			hires: 'border-primary/70 text-primary',
			lossless: 'border-foreground/40 text-foreground/80',
			lossy: 'border-muted-foreground/40 text-muted-foreground'
		},
		art: {
			hires: 'border-transparent bg-primary text-primary-foreground',
			lossless: 'border-white/70 text-white',
			lossy: 'border-white/40 text-white/70'
		}
	} as const;
</script>

{#if parts.length}
	<span
		class="inline-flex min-w-0 items-center gap-1.5 whitespace-nowrap text-[10px] leading-none tabular-nums {tone ===
		'art'
			? 'text-white/70'
			: 'text-muted-foreground'} {klass}"
		title="{label}{SEPARATOR}{text}"
	>
		<span
			class="shrink-0 rounded-[3px] border px-1 py-0.5 text-[9px] font-semibold tracking-wider {BADGE[tone][
				tier
			]}">{label}</span
		>
		{#if !badgeOnly}
			<span class="truncate">{text}</span>
		{/if}
	</span>
{/if}
