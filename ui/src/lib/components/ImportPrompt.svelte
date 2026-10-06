<script lang="ts">
	// The first-run import prompt (R2, D8): "We found data from LiMusic / PlaylistForge. Import it?"
	// Asked once, and again only when a new source appears (onboarding.ts). "Now" opens Settings ▸
	// Import & migrate, where the actual copy is started; nothing is imported from here.
	//
	// Also the first thing to read a finished migration's result after the restart: it opens the
	// same tab on it, which is where the outcome and the autostart offer are shown.
	import { onMount } from 'svelte';
	import * as Dialog from '$lib/components/ui/dialog';
	import { Button } from '$lib/components/ui/button';
	import * as api from '$lib/api';
	import { ui } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';
	import {
		answerValue,
		pendingNotice,
		shouldPrompt,
		type ImportSourceId,
		type PromptAnswer
	} from '$lib/onboarding';

	let open = $state(false);
	let detected = $state<ImportSourceId[]>([]);
	let stored: string | undefined;
	let answered = false;

	// A migration that is still pending: past its ten-minute window, or not done on the last launch
	// (LiMusic open, a file held). Asked here rather than by opening Settings on every start.
	let pending = $state<api.MigratePending | null>(null);
	let pendingKind = $state<'expired' | 'retry' | null>(null);
	let pendingOpen = $state(false);
	let pendingBusy = $state(false);
	let pendingError = $state('');

	onMount(async () => {
		try {
			const result = await api.migrateUpstreamResult();
			const marker = await api.migrateUpstreamPending().catch(() => null);
			const kind = pendingNotice(marker);
			if (kind) {
				pending = marker;
				pendingKind = kind;
				pendingOpen = true;
				return;
			}
			if (result) {
				ui.importResult = result;
				ui.settingsFocus = { tab: 'import' };
				ui.settingsOpen = true;
				return;
			}
			const [sources, settings] = await Promise.all([api.importSources(), api.getSettings()]);
			const found: ImportSourceId[] = [];
			if (sources.upstream) found.push('limusic');
			if (sources.playlistforge) found.push('playlistforge');
			stored = settings.onboarding_import_prompted;
			if (shouldPrompt(stored, found)) {
				detected = found;
				open = true;
			}
		} catch {
			// Detection failing is no reason to bother anyone at startup; Settings still has it.
		}
	});

	async function answer(a: PromptAnswer) {
		answered = true;
		open = false;
		try {
			await api.setSetting('onboarding_import_prompted', answerValue(a, detected, stored));
		} catch {
			// Not stored: it asks again next launch, which is the safe way to fail.
		}
		if (a === 'now') {
			ui.settingsFocus = { tab: 'import' };
			ui.settingsOpen = true;
		}
	}

	async function retryPending() {
		if (!pending) return;
		pendingError = '';
		pendingBusy = true;
		try {
			// A fresh marker and a restart: nothing after this line runs on success.
			await api.migrateUpstreamRequest(pending.include_webview);
		} catch (e) {
			pendingBusy = false;
			pendingError =
				String(e) === 'upstream_running' ? t('settings.import.limusic_running') : String(e);
		}
	}

	async function cancelPending() {
		pendingError = '';
		pendingBusy = true;
		try {
			await api.migrateUpstreamCancel();
			pendingOpen = false;
			pending = null;
		} catch (e) {
			pendingError = String(e);
		} finally {
			pendingBusy = false;
		}
	}

	const names = $derived(
		detected
			.map((s) => (s === 'limusic' ? t('onboarding.import.limusic') : t('onboarding.import.playlistforge')))
			.join(t('onboarding.import.and'))
	);
</script>

<!-- Closing it any other way (Escape, the overlay) counts as "later": it was seen. -->
<Dialog.Root
	bind:open
	onOpenChange={(o) => {
		if (!o && !answered) answer('later');
	}}
>
	<Dialog.Content class="sm:max-w-md">
		<Dialog.Header>
			<Dialog.Title>{t('onboarding.import.title', { sources: names })}</Dialog.Title>
			<Dialog.Description>{t('onboarding.import.body')}</Dialog.Description>
		</Dialog.Header>
		<Dialog.Footer>
			<Button variant="ghost" onclick={() => answer('no')}>{t('onboarding.import.no')}</Button>
			<Button variant="outline" onclick={() => answer('later')}>{t('onboarding.import.later')}</Button>
			<Button onclick={() => answer('now')}>{t('onboarding.import.now')}</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>

<!-- Closing it without choosing leaves the marker as it is: it is asked again on the next start,
     and Settings ▸ Import & migrate offers the same two buttons. -->
<Dialog.Root bind:open={pendingOpen}>
	<Dialog.Content class="sm:max-w-md">
		<Dialog.Header>
			<Dialog.Title>{t('settings.import.pending_title')}</Dialog.Title>
			<Dialog.Description>
				{pendingKind === 'expired'
					? t('settings.import.pending_expired')
					: t('settings.import.pending_retry')}
			</Dialog.Description>
		</Dialog.Header>
		{#if pendingError}
			<p class="text-xs text-destructive">{pendingError}</p>
		{/if}
		<Dialog.Footer>
			<Button variant="outline" disabled={pendingBusy} onclick={cancelPending}>
				{t('settings.import.pending_cancel')}
			</Button>
			<Button disabled={pendingBusy} onclick={retryPending}>
				{pendingBusy ? t('common.loading') : t('settings.import.pending_retry_now')}
			</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
