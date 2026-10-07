<script lang="ts">
	// Settings ▸ YouTube Data API, top: where the Data API stands right now (`ytdata_status`), always
	// shown here, unlike `YtDataWarning`, which stays quiet about an API nobody asked for.
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Alert02Icon, CheckmarkCircle02Icon } from '@hugeicons/core-free-icons';
	import { t } from '$lib/i18n.svelte';
	import { refreshYtData, trackYtData, ytdata } from '$lib/ytdata.svelte';

	onMount(() => {
		trackYtData();
		void refreshYtData();
	});

	const status = $derived(ytdata.status);
	const account = $derived(status?.account_title ?? t('ytdata.your_channel'));
	const quota = $derived.by(() => {
		if (!status) return '';
		const reset = new Date(status.next_reset).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
		return t('ytdata.quota_detail', { spent: status.spent_today, daily: status.daily_units, reset });
	});
</script>

{#if status}
	<div
		role="status"
		class="flex items-start gap-3 rounded-xl border p-3 text-sm {status.state === 'ok'
			? 'bg-card'
			: 'border-amber-500/30 bg-amber-500/5'}"
	>
		<HugeiconsIcon
			icon={status.state === 'ok' ? CheckmarkCircle02Icon : Alert02Icon}
			class="mt-0.5 h-4 w-4 shrink-0 {status.state === 'ok' ? 'text-primary' : 'text-amber-500'}"
		/>
		<div class="min-w-0 flex-1">
			<p class="font-medium">
				{status.state === 'ok'
					? t('ytdata.settings.ok', { account })
					: t(`ytdata.state.${status.state}`)}
			</p>
			{#if status.reason}
				<p class="mt-0.5 text-xs text-muted-foreground">{t(`ytdata.reason.${status.reason}`, { account })}</p>
			{/if}
			<p class="mt-0.5 text-xs text-muted-foreground">{quota}</p>
		</div>
	</div>
{/if}
