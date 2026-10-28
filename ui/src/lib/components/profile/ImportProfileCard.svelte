<script lang="ts">
	// Settings ▸ Import & migrate ▸ Profile ▸ import. Pick a .lmfprofile, read its manifest
	// (nothing is changed), decide what to do with each absolute-path root, confirm. Rust then
	// stages the import and restarts; the result is read after the restart (ProfileImportResult).
	import { open } from '@tauri-apps/plugin-dialog';
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Alert02Icon, CheckmarkCircle02Icon } from '@hugeicons/core-free-icons';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Alert, AlertDescription } from '$lib/components/ui/alert';
	import * as Dialog from '$lib/components/ui/dialog';
	import * as api from '$lib/api';
	import { t, type TranslationKey } from '$lib/i18n.svelte';
	import { profileErrorText } from './profile-errors';

	const CARD = 'divide-y divide-border/60 overflow-hidden rounded-xl border bg-card';
	const ROW = 'px-4 py-3';
	const HINT = 'text-xs leading-relaxed text-muted-foreground';
	const SELECT =
		'h-9 rounded-md border border-input bg-background px-2 text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring';

	type Choice = { action: 'map' | 'keep' | 'drop'; to: string };

	let path = $state<string | null>(null);
	let preview = $state<api.ProfilePreview | null>(null);
	let reading = $state(false);
	let error = $state('');
	let pass = $state('');
	let verifying = $state(false);
	let choices = $state<Record<string, Choice>>({});
	let confirmOpen = $state(false);
	let importing = $state(false);
	let importError = $state('');
	let pendingMarker = $state(false);

	const m = $derived(preview?.manifest ?? null);
	const needsPass = $derived(!!m?.includes_secrets);
	const unlocked = $derived(!needsPass || preview?.secrets_ok === true);
	const mapMissing = $derived(
		Object.values(choices).some((c) => c.action === 'map' && c.to.trim() === '')
	);
	const anyDrop = $derived(Object.values(choices).some((c) => c.action === 'drop'));
	const canImport = $derived(!!preview && unlocked && !mapMissing && !importing);

	onMount(() => {
		api.profileImportPending().then((p) => (pendingMarker = p)).catch(() => {});
	});

	function initChoices(p: api.ProfilePreview) {
		choices = Object.fromEntries(
			p.roots.map((r) => [
				r.root,
				r.suggestion
					? { action: 'map' as const, to: r.suggestion }
					: { action: 'keep' as const, to: '' }
			])
		);
	}

	async function chooseFile() {
		let file: string | string[] | null = null;
		try {
			file = await open({
				multiple: false,
				directory: false,
				filters: [{ name: t('settings.profile.file_filter'), extensions: ['lmfprofile'] }]
			});
		} catch (e) {
			error = String(e);
			return;
		}
		if (typeof file !== 'string') return;
		path = file;
		pass = '';
		preview = null;
		error = '';
		importError = '';
		reading = true;
		try {
			const p = await api.profilePreview(file, null);
			preview = p;
			initChoices(p);
		} catch (e) {
			error = profileErrorText(e);
		} finally {
			reading = false;
		}
	}

	async function verify() {
		if (!path || !pass) return;
		error = '';
		verifying = true;
		try {
			const p = await api.profilePreview(path, pass);
			preview = p;
			// Keep what was already chosen for the roots that are still there.
			const prev = choices;
			initChoices(p);
			for (const k of Object.keys(choices)) if (prev[k]) choices[k] = prev[k];
		} catch (e) {
			error = profileErrorText(e);
		} finally {
			verifying = false;
		}
	}

	async function pickFolder(root: string) {
		try {
			const dir = await open({
				directory: true,
				multiple: false,
				title: t('settings.profile.pick_folder_title')
			});
			if (typeof dir === 'string') choices[root] = { action: 'map', to: dir };
		} catch {
			// Cancelled or no dialog: the text field still works.
		}
	}

	function decisions(): api.RootDecision[] {
		return Object.entries(choices).map(([root, c]): api.RootDecision =>
			c.action === 'map'
				? { root, action: 'map', to: c.to.trim() }
				: { root, action: c.action }
		);
	}

	async function runImport() {
		if (!path) return;
		importError = '';
		importing = true;
		try {
			// Stages the import and restarts the app: nothing after this line runs on success.
			await api.profileImportRequest(path, needsPass ? pass : null, decisions());
		} catch (e) {
			importing = false;
			confirmOpen = false;
			importError = profileErrorText(e);
			api.profileImportPending().then((p) => (pendingMarker = p)).catch(() => {});
		}
	}

	async function cancelPending() {
		try {
			await api.profileImportCancel();
		} catch (e) {
			importError = profileErrorText(e);
		}
		pendingMarker = await api.profileImportPending().catch(() => false);
	}

	const osLabel = (os: string) =>
		os === 'windows' ? 'Windows' : os === 'macos' ? 'macOS' : os === 'linux' ? 'Linux' : os;
	const dateLabel = (iso: string) => {
		const d = new Date(iso);
		return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
	};
	const kindLabel = (k: string) => {
		const key = `settings.profile.kind.${k}` as TranslationKey;
		const text = t(key);
		return text === key ? k : text;
	};
</script>

<div class={CARD}>
	<div class="{ROW} space-y-1">
		<p class="text-sm font-medium">{t('settings.profile.import_title')}</p>
		<p class={HINT}>{t('settings.profile.import_hint')}</p>
	</div>

	{#if pendingMarker}
		<div class={ROW}>
			<Alert>
				<HugeiconsIcon icon={Alert02Icon} size={16} strokeWidth={1.8} />
				<AlertDescription>
					<p>{t('settings.profile.pending')}</p>
					<Button size="sm" variant="outline" class="mt-2" onclick={cancelPending}>
						{t('settings.profile.pending_cancel')}
					</Button>
				</AlertDescription>
			</Alert>
		</div>
	{/if}

	<div class="{ROW} flex flex-wrap items-center gap-3">
		<Button size="sm" variant="outline" disabled={reading || importing} onclick={chooseFile}>
			{reading ? t('common.loading') : t('settings.profile.choose_file')}
		</Button>
		{#if path}<span class="min-w-0 break-all text-[11px] text-muted-foreground">{path}</span>{/if}
	</div>

	{#if error}
		<div class={ROW}><p class="text-xs text-destructive">{error}</p></div>
	{/if}

	{#if preview && m}
		<div class="{ROW} space-y-1">
			<p class="text-sm font-medium">
				{t('settings.profile.source', { os: osLabel(m.source_os), date: dateLabel(m.created_at) })}
			</p>
			<p class={HINT}>
				{t('settings.profile.counts', {
					playlists: m.counts.local_playlists,
					tracks: m.counts.local_tracks,
					listens: m.counts.listens,
					accounts: m.counts.accounts,
					downloads: m.counts.downloads,
					settings: m.counts.settings
				})}
			</p>
			<p class="text-[11px] text-muted-foreground">
				{t('settings.profile.app_version', { version: m.app_version })}
				{m.includes_caches ? ' · ' + t('settings.profile.has_caches') : ''}
			</p>
		</div>

		{#if needsPass}
			<div class="{ROW} space-y-2">
				<p class="text-sm font-medium">{t('settings.profile.has_secrets')}</p>
				{#if m.secret_counts}
					<p class={HINT}>
						{t('settings.profile.secret_counts', {
							sessions: m.secret_counts.google_sessions,
							tokens: m.secret_counts.ytdata_tokens
						})}
						{[
							m.secret_counts.client_secret ? t('settings.profile.secret_client') : '',
							m.secret_counts.lastfm ? 'Last.fm' : '',
							m.secret_counts.listenbrainz ? 'ListenBrainz' : '',
							m.secret_counts.proxy ? 'proxy' : ''
						]
							.filter(Boolean)
							.join(' · ')}
					</p>
				{/if}
				{#if preview.secrets_ok}
					<p class="flex items-center gap-2 text-xs">
						<HugeiconsIcon icon={CheckmarkCircle02Icon} size={14} strokeWidth={1.8} />
						{t('settings.profile.passphrase_verified')}
					</p>
				{:else}
					<p class={HINT}>{t('settings.profile.passphrase_required_hint')}</p>
					<form
						class="flex flex-wrap items-center gap-2"
						onsubmit={(e) => {
							e.preventDefault();
							verify();
						}}
					>
						<Input
							type="password"
							autocomplete="off"
							class="max-w-xs"
							placeholder={t('settings.profile.passphrase')}
							aria-label={t('settings.profile.passphrase')}
							bind:value={pass}
							disabled={verifying}
						/>
						<Button type="submit" size="sm" variant="outline" disabled={verifying || !pass}>
							{verifying ? t('common.loading') : t('settings.profile.verify')}
						</Button>
					</form>
				{/if}
			</div>
		{:else}
			<div class={ROW}><p class={HINT}>{t('settings.profile.no_secrets')}</p></div>
		{/if}

		{#if preview.roots.length > 0}
			<div class="{ROW} space-y-3">
				<div>
					<p class="text-sm font-medium">{t('settings.profile.roots_title')}</p>
					<p class={HINT}>{t('settings.profile.roots_hint')}</p>
				</div>
				{#each preview.roots as r (r.root)}
					{@const c = choices[r.root]}
					{#if c}
						<div class="space-y-2 rounded-lg border border-border/60 p-3">
							<div class="flex flex-wrap items-baseline justify-between gap-2">
								<span class="break-all font-mono text-xs">{r.root}</span>
								<span class="text-[11px] text-muted-foreground">
									{t('settings.profile.root_refs', { count: r.count })} · {r.kinds
										.map(kindLabel)
										.join(', ')}
								</span>
							</div>
							<select class={SELECT} bind:value={c.action} aria-label={r.root} disabled={importing}>
								<option value="map">{t('settings.profile.action_map')}</option>
								<option value="keep">{t('settings.profile.action_keep')}</option>
								<option value="drop">{t('settings.profile.action_drop')}</option>
							</select>
							{#if c.action === 'map'}
								<div class="flex flex-wrap items-center gap-2">
									<Input
										class="min-w-0 flex-1 font-mono text-xs"
										bind:value={c.to}
										placeholder={t('settings.profile.map_placeholder')}
										aria-label={t('settings.profile.map_placeholder')}
										disabled={importing}
									/>
									<Button
										size="sm"
										variant="outline"
										disabled={importing}
										onclick={() => pickFolder(r.root)}
									>
										{t('settings.profile.pick_folder')}
									</Button>
								</div>
							{:else if c.action === 'keep'}
								<p class={HINT}>{t('settings.profile.keep_hint')}</p>
							{:else}
								<p class="text-xs text-destructive">{t('settings.profile.drop_hint')}</p>
							{/if}
						</div>
					{/if}
				{/each}
			</div>
		{/if}

		<div class="{ROW} flex items-center gap-3">
			<Button size="sm" disabled={!canImport} onclick={() => (confirmOpen = true)}>
				{t('settings.profile.import_button')}
			</Button>
			{#if needsPass && !unlocked}<span class={HINT}>{t('settings.profile.verify_first')}</span>{/if}
		</div>
	{/if}

	{#if importError}
		<div class={ROW}><p class="text-xs text-destructive">{importError}</p></div>
	{/if}
</div>

<Dialog.Root bind:open={confirmOpen}>
	<Dialog.Content class="sm:max-w-md">
		<Dialog.Header>
			<Dialog.Title>{t('settings.profile.confirm_title')}</Dialog.Title>
			<Dialog.Description>{t('settings.profile.confirm_body')}</Dialog.Description>
		</Dialog.Header>
		{#if anyDrop}
			<p class="text-xs text-destructive">{t('settings.profile.confirm_drop')}</p>
		{/if}
		<Dialog.Footer>
			<Button variant="outline" disabled={importing} onclick={() => (confirmOpen = false)}>
				{t('settings.profile.cancel')}
			</Button>
			<Button disabled={importing} onclick={runImport}>
				{importing ? t('settings.profile.importing') : t('settings.profile.confirm_button')}
			</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
