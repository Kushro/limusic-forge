<script lang="ts">
	// Settings ▸ YouTube Data API: the local echo's freshness window (`jobs/local_echo.rs`, port of
	// PlaylistForge's eight-position slider). When a Data API change finishes, the playlist on this
	// computer is edited to match at once, instead of at the next sync, but only when that playlist
	// synced within the window. Stored in `jobs.local_echo_max_age_s`: 0 off, -1 always, else
	// seconds. A stored value between two positions shows on the safer one, like `slider_index`.
	import { Slider } from '$lib/components/ui/slider';
	import * as api from '$lib/api';
	import { t } from '$lib/i18n.svelte';

	let { settings }: { settings: Record<string, string> } = $props();

	const KEY = 'jobs.local_echo_max_age_s';
	/** Least to most risky, left to right: `EchoWindow::SLIDER`. */
	const POSITIONS = [0, 15 * 60, 30 * 60, 3600, 3 * 3600, 12 * 3600, 24 * 3600, -1];
	const DEFAULT_SECONDS = 3600;

	/** `EchoWindow::parse` then `slider_index`. */
	function indexOf(raw: string | undefined): number {
		const trimmed = raw?.trim() ?? '';
		const n = /^-?\d+$/.test(trimmed) ? Number(trimmed) : DEFAULT_SECONDS;
		if (n === 0) return 0;
		if (n < 0) return POSITIONS.length - 1;
		let at = -1;
		POSITIONS.forEach((p, i) => {
			if (p > 0 && p <= n) at = i;
		});
		return at === -1 ? 1 : at;
	}

	function label(i: number): string {
		const s = POSITIONS[i];
		if (s === 0) return t('ytdata.echo.off');
		if (s < 0) return t('ytdata.echo.always');
		return s < 3600 ? t('ytdata.echo.minutes', { n: s / 60 }) : t('ytdata.echo.hours', { n: s / 3600 });
	}

	const index = $derived(indexOf(settings[KEY]));
	const summary = $derived.by(() => {
		const s = POSITIONS[index];
		if (s === 0) return t('ytdata.echo.current_off');
		if (s < 0) return t('ytdata.echo.current_always');
		return t('ytdata.echo.current', { window: label(index) });
	});

	let error = $state('');

	async function choose(i: number) {
		const value = String(POSITIONS[i]);
		if (value === settings[KEY]) return;
		error = '';
		const before: string | undefined = settings[KEY];
		settings[KEY] = value;
		try {
			await api.setSetting(KEY, value);
		} catch (e) {
			if (before === undefined) delete settings[KEY];
			else settings[KEY] = before;
			error = String(e);
		}
	}
</script>

<div class="overflow-hidden rounded-xl border bg-card px-4 py-3.5">
	<div class="flex items-center justify-between gap-6">
		<span class="text-sm font-medium">{t('ytdata.echo.label')}</span>
		<span class="text-xs font-medium text-primary">{label(index)}</span>
	</div>
	<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">{t('ytdata.echo.hint')}</p>
	<div class="mt-4">
		<Slider
			type="single"
			aria-label={t('ytdata.echo.label')}
			min={0}
			max={POSITIONS.length - 1}
			step={1}
			value={index}
			onValueCommit={choose}
		/>
		<div class="mt-1.5 flex justify-between text-[11px] text-muted-foreground">
			<span>{t('ytdata.echo.safer')}</span>
			<span>{t('ytdata.echo.riskier')}</span>
		</div>
	</div>
	<p class="mt-2 text-xs text-muted-foreground">{summary}</p>
	{#if error}
		<p class="mt-2 text-xs text-destructive" role="status">{error}</p>
	{/if}
</div>
