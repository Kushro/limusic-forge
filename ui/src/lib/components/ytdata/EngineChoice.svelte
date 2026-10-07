<script lang="ts">
	// The engine for one playlist write, and what it costs: shown in the drop popover and the tools
	// dialog. Until the user picks one, `choice` stays unset and the write follows `playlist_engine`
	// (the caller passes `choice` on, `api.withEngineChoice` / `api.setEngineChoice`, so an untouched
	// selector changes nothing). When the write would go through the Data API, or could (the engine
	// is auto and the API works), the line reads "≈ N u of M available" from `estimate_op`. Over the
	// budget it says what then happens: auto falls back to InnerTube, an explicit Data API choice
	// waits in the queue for the quota. A write the Data API cannot make at all (Liked Music, an
	// album, no channel for this account) goes through InnerTube whatever the choice, and says so.
	import { onMount } from 'svelte';
	import * as api from '$lib/api';
	import { t } from '$lib/i18n.svelte';
	import { showsCost, trackYtData, ytdata, type PlaylistEngine } from '$lib/ytdata.svelte';

	type Priced = { label?: string; kind: 'copy' | 'move' | 'remove'; rows: number; playlists: string[] };

	let {
		choice = $bindable(),
		ops,
		perTrack = false
	}: {
		/** The engine the user picked for this write; unset follows `playlist_engine`. */
		choice?: PlaylistEngine;
		/** The writes to price: one, or a copy and a move side by side. */
		ops: Priced[];
		/** The rows are not known yet (the tools dialog): price one and say it is per track. */
		perTrack?: boolean;
	} = $props();

	onMount(trackYtData);

	const CHOICES: PlaylistEngine[] = ['auto', 'ytdata', 'innertube'];
	const selected = $derived<PlaylistEngine>(choice ?? ytdata.engine);
	let estimates = $state<(api.OpEstimate | null)[]>([]);

	$effect(() => {
		const engine = selected;
		const wanted = ops.map((o) => ({ kind: o.kind, rows: perTrack ? 1 : o.rows, playlists: [...o.playlists] }));
		let stale = false;
		Promise.all(
			wanted.map((o) =>
				api.estimateOp(o.kind, { rows: o.rows, playlists: o.playlists, engine }).catch(() => null)
			)
		).then((r) => {
			if (!stale) estimates = r;
		});
		return () => {
			stale = true;
		};
	});

	const known = $derived(estimates.filter((e): e is api.OpEstimate => e !== null));
	/** Every write is to a playlist on this computer: no engine to pick, nothing to spend. */
	const local = $derived(known.length > 0 && known.every((e) => e.engine === null));
	const show = $derived(!local && known.length > 0 && showsCost(ytdata.status, selected));
	const available = $derived(known[0]?.available ?? 0);
	const note = $derived.by(() => {
		if (selected === 'innertube' || perTrack) return '';
		const viaYtdata = known.filter((e) => e.engine === 'ytdata');
		if (selected === 'ytdata') {
			if (known.some((e) => e.engine === 'innertube')) return t('ytdata.estimate.unavailable');
			if (viaYtdata.some((e) => e.units > e.available)) return t('ytdata.estimate.over_ytdata');
			return '';
		}
		// Auto took InnerTube because the Data API works but the estimate does not fit.
		const overflowed = known.some((e) => e.engine === 'innertube' && e.units > e.available);
		return overflowed && ytdata.status?.state === 'ok' ? t('ytdata.estimate.over_auto') : '';
	});
	const costLine = $derived.by(() => {
		const priced = ops.map((o, i) => ({ o, e: estimates[i] ?? null })).filter((p) => p.e !== null);
		if (perTrack) return t('ytdata.estimate.per_track', { units: priced[0]?.e?.units ?? 0, available });
		if (priced.length === 1) return t('ytdata.estimate.cost', { units: priced[0].e?.units ?? 0, available });
		const parts = priced.map((p) => `${p.o.label ?? ''} ≈ ${p.e?.units ?? 0} u`).join(' · ');
		return t('ytdata.estimate.cost_many', { parts, available });
	});
</script>

{#if !local}
	<div class="mt-3">
		<p class="text-xs font-medium text-muted-foreground">{t('ytdata.estimate.engine')}</p>
		<div
			class="mt-1 grid grid-cols-3 gap-1 rounded-lg bg-muted/60 p-0.5"
			role="radiogroup"
			aria-label={t('ytdata.estimate.engine')}
		>
			{#each CHOICES as c (c)}
				<button
					type="button"
					role="radio"
					aria-checked={selected === c}
					class="rounded-md px-2 py-1 text-xs font-medium transition-colors {selected === c
						? 'bg-background text-foreground shadow-sm'
						: 'text-muted-foreground hover:text-foreground'}"
					onclick={() => (choice = c)}
				>
					{t(`ytdata.estimate.${c}`)}
				</button>
			{/each}
		</div>
		{#if show}
			<p class="mt-1.5 text-xs text-muted-foreground">{costLine}</p>
		{/if}
		{#if note}
			<p class="mt-1 text-xs text-amber-500">{note}</p>
		{/if}
	</div>
{/if}
