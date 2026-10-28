<script lang="ts">
	// After an imported profile has been applied and the app restarted: what came in, once. Offers
	// to register the scheduled monitor again (the task lives in the OS, not in the profile) and to
	// turn autostart back on.
	import { onMount } from 'svelte';
	import * as Dialog from '$lib/components/ui/dialog';
	import { Button } from '$lib/components/ui/button';
	import * as api from '$lib/api';
	import { t, type TranslationKey } from '$lib/i18n.svelte';
	import { toast } from '$lib/player.svelte';

	let open = $state(false);
	let r = $state<api.ProfileImportReport | null>(null);
	let canMonitor = $state(false);
	let monitorDone = $state(false);
	let monitorBusy = $state(false);
	let monitorError = $state('');
	let autostartDone = $state(false);
	let autostartBusy = $state(false);

	onMount(async () => {
		try {
			const report = await api.profileImportResult();
			if (!report) return;
			r = report;
			if (report.monitor_time) {
				canMonitor = (await api.wintaskStatus().catch(() => null))?.supported === true;
			}
			open = true;
		} catch {
			// No result to show is the normal case.
		}
	});

	const failed = $derived(r?.tokens?.failed ?? []);
	const keyringFailed = $derived(failed.some(([, code]) => code === 'keyring_unavailable'));
	const mismatch = $derived(failed.some(([, code]) => code === 'client_mismatch'));
	const otherFailed = $derived(
		failed.filter(([, code]) => code !== 'keyring_unavailable' && code !== 'client_mismatch').length
	);
	const warnings = $derived(r?.warnings ?? []);
	const lines = $derived.by(() => {
		const out: string[] = [];
		const c = r?.imported;
		if (c) {
			if (c.local_playlists) out.push(t('settings.profile.res_playlists', { count: c.local_playlists }));
			if (c.local_tracks) out.push(t('settings.profile.res_tracks', { count: c.local_tracks }));
			if (c.listens) out.push(t('settings.profile.res_listens', { count: c.listens }));
			if (c.accounts) out.push(t('settings.profile.res_accounts', { count: c.accounts }));
			if (c.downloads) out.push(t('settings.profile.res_downloads', { count: c.downloads }));
		}
		const m = r?.remap;
		if (m) {
			if (m.rewritten) out.push(t('settings.profile.res_rewritten', { count: m.rewritten }));
			if (m.kept) out.push(t('settings.profile.res_kept', { count: m.kept }));
			if (m.dropped) out.push(t('settings.profile.res_dropped', { count: m.dropped }));
		}
		const saved = r?.tokens?.saved.length ?? 0;
		if (saved) out.push(t('settings.profile.res_tokens', { count: saved }));
		out.push(r?.secrets_restored ? t('settings.profile.res_secrets_yes') : t('settings.profile.res_secrets_no'));
		return out;
	});

	function warningText(code: string): string {
		const key = `settings.profile.warnings.${code}` as TranslationKey;
		const text = t(key);
		return text === key ? code : text;
	}

	async function registerMonitor() {
		if (!r?.monitor_time) return;
		monitorError = '';
		monitorBusy = true;
		try {
			await api.wintaskRegister(r.monitor_time);
			monitorDone = true;
		} catch (e) {
			monitorError = String(e);
		} finally {
			monitorBusy = false;
		}
	}

	async function enableAutostart() {
		autostartBusy = true;
		try {
			await api.setSetting('autostart', 'true');
			autostartDone = true;
		} catch (e) {
			toast.error(String(e));
		} finally {
			autostartBusy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content class="sm:max-w-lg">
		<Dialog.Header>
			<Dialog.Title>
				{r?.error ? t('settings.profile.result_failed') : t('settings.profile.result_title')}
			</Dialog.Title>
			<Dialog.Description>
				{r?.error
					? t('settings.profile.result_failed_body', { error: r.error })
					: t('settings.profile.result_body')}
			</Dialog.Description>
		</Dialog.Header>
		{#if r && !r.error}
			<ul class="list-disc space-y-0.5 pl-5 text-sm">
				{#each lines as line}<li>{line}</li>{/each}
			</ul>
			{#if keyringFailed}
				<p class="text-xs text-destructive">{t('settings.profile.res_keyring')}</p>
			{/if}
			{#if mismatch}
				<p class="text-xs text-destructive">{t('settings.profile.res_client_mismatch')}</p>
			{/if}
			{#if otherFailed > 0}
				<p class="text-xs text-destructive">
					{t('settings.profile.res_tokens_failed', { count: otherFailed })}
				</p>
			{/if}
			{#if warnings.length > 0}
				<ul class="list-disc space-y-0.5 pl-5 text-xs text-muted-foreground">
					{#each warnings as w}<li>{warningText(w)}</li>{/each}
				</ul>
			{/if}
			{#if r.monitor_time && canMonitor}
				<div class="flex flex-wrap items-center gap-3 rounded-lg border border-border/60 p-3">
					<span class="min-w-0 flex-1 text-xs">{t('settings.profile.res_monitor')}</span>
					{#if monitorDone}
						<span class="text-xs">{t('settings.profile.res_monitor_done')}</span>
					{:else}
						<Button size="sm" variant="outline" disabled={monitorBusy} onclick={registerMonitor}>
							{t('settings.profile.res_monitor_button', { time: r.monitor_time })}
						</Button>
					{/if}
					{#if monitorError}<p class="w-full text-xs text-destructive">{monitorError}</p>{/if}
				</div>
			{/if}
			{#if r.autostart_was_on}
				<div class="flex flex-wrap items-center gap-3 rounded-lg border border-border/60 p-3">
					<span class="min-w-0 flex-1 text-xs">{t('settings.profile.res_autostart')}</span>
					{#if autostartDone}
						<span class="text-xs">{t('settings.profile.res_autostart_done')}</span>
					{:else}
						<Button size="sm" variant="outline" disabled={autostartBusy} onclick={enableAutostart}>
							{t('settings.profile.res_autostart_button')}
						</Button>
					{/if}
				</div>
			{/if}
		{/if}
		<Dialog.Footer>
			<Button onclick={() => (open = false)}>{t('settings.profile.close')}</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
