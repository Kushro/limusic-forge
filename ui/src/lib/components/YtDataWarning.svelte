<script lang="ts">
	// Why the YouTube Data API can't be used right now (src-tauri/src/ytdata_status.rs), shown where
	// its functions are: one line with the reason, the quota counts when it ran out, and "Configure",
	// which opens Settings ▸ YouTube Data API (`YTDATA_TAB`). Shows nothing when the API works, and
	// nothing for an API nobody set up unless the engine asks for it (`shouldWarn`): with `auto` or
	// `innertube` the work simply goes through InnerTube. `hideConfigure` drops the button where the
	// warning already sits in that tab.
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Alert02Icon } from '@hugeicons/core-free-icons';
	import { ui } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';
	import { shouldWarn, trackYtData, ytdata, YTDATA_TAB } from '$lib/ytdata.svelte';
	import { Button } from './ui/button';

	let { compact = false, hideConfigure = false }: { compact?: boolean; hideConfigure?: boolean } =
		$props();

	onMount(trackYtData);

	const status = $derived(ytdata.status);
	const show = $derived(shouldWarn(status, ytdata.engine));
	const account = $derived(status?.account_title ?? t('ytdata.your_channel'));
	const headline = $derived(status && status.state !== 'ok' ? t(`ytdata.state.${status.state}`) : '');
	const detail = $derived.by(() => {
		if (!status) return '';
		if (status.reason) return t(`ytdata.reason.${status.reason}`, { account });
		if (status.state === 'quota_exhausted') {
			const reset = new Date(status.next_reset).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
			return t('ytdata.quota_detail', { spent: status.spent_today, daily: status.daily_units, reset });
		}
		return '';
	});

	function configure() {
		ui.settingsFocus = { tab: YTDATA_TAB };
		ui.settingsOpen = true;
	}
</script>

{#if show && status}
	<div
		role="status"
		class="flex items-start gap-3 rounded-xl border border-amber-500/30 bg-amber-500/5 text-sm {compact ? 'px-3 py-2' : 'mb-4 p-3'}"
	>
		<HugeiconsIcon icon={Alert02Icon} class="mt-0.5 h-4 w-4 shrink-0 text-amber-500" />
		<div class="min-w-0 flex-1">
			<p class="font-medium">{headline}</p>
			{#if detail && !compact}
				<p class="mt-0.5 text-xs text-muted-foreground">{detail}</p>
			{/if}
		</div>
		{#if !hideConfigure}
			<Button variant="outline" size="sm" class="shrink-0" onclick={configure} title={compact ? detail : undefined}>
				{t('ytdata.configure')}
			</Button>
		{/if}
	</div>
{/if}
