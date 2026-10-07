<script lang="ts">
	// Settings ▸ Import & migrate ▸ PlaylistForge (commit 31): detect → preview → import with
	// progress → summary → offer to remove PlaylistForge's daily task. Rust does the work
	// (src-tauri/src/pf_import/); this only gathers the choices. Nothing secret comes this way:
	// counts, names and short codes.
	import { onDestroy, onMount } from 'svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import { setMode } from 'mode-watcher';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Alert02Icon, DatabaseImportIcon } from '@hugeicons/core-free-icons';
	import { Button } from '$lib/components/ui/button';
	import { Alert, AlertDescription } from '$lib/components/ui/alert';
	import * as api from '$lib/api';
	import { t, setLocale, type LocaleId, type TranslationKey } from '$lib/i18n.svelte';
	import { applyTheme, type ThemeId } from '$lib/theme.svelte';
	import { refreshLocalPlaylists, ui } from '$lib/player.svelte';
	import { imp } from '$lib/import.svelte';

	const CARD = 'divide-y divide-border/60 overflow-hidden rounded-xl border bg-card';
	const ROW = 'px-4 py-3';
	const HINT = 'text-xs leading-relaxed text-muted-foreground';
	const CHECK = 'size-4 shrink-0 accent-primary';

	let detect = $state<api.PfDetect | null>(null);
	let detecting = $state(true);
	/** A folder picked by hand, when PlaylistForge is not in its default place. */
	let path = $state<string | null>(null);
	let preview = $state<api.PfPreview | null>(null);
	let reading = $state(false);
	let error = $state('');

	// The choices.
	let picked = $state<Record<string, boolean>>({});
	let accountMap = $state<Record<string, string | null>>({});
	let missingAs = $state<api.PfMissingAs>('local');
	let alerts = $state(true);
	let downloads = $state(true);
	let settings = $state(true);
	let appearance = $state(true);
	let dataApi = $state(true);
	let tokens = $state(false);

	// The run.
	let importing = $state(false);
	let progress = $state<api.PfProgress | null>(null);
	let result = $state<api.PfApplyResult | null>(null);
	let tokenOutcomes = $state<api.PfTokenOutcome[]>([]);

	// The task.
	let confirmTask = $state(false);
	let taskBusy = $state(false);
	let taskDone = $state(false);
	let taskError = $state('');

	function errorText(e: unknown): string {
		const raw = String(e ?? '');
		const key = `pf_import.errors.${raw}` as TranslationKey;
		const text = t(key);
		return text === key ? raw : text;
	}

	async function runDetect() {
		detecting = true;
		error = '';
		try {
			detect = await api.pfDetect();
		} catch (e) {
			error = errorText(e);
		} finally {
			detecting = false;
		}
	}

	let unlisten: (() => void) | undefined;
	onMount(() => {
		runDetect();
		api.onPfImportProgress((p) => (progress = p)).then((u) => (unlisten = u));
	});
	onDestroy(() => unlisten?.());

	async function chooseFolder() {
		const dir = await open({ directory: true, title: t('pf_import.choose_folder_title') });
		if (typeof dir !== 'string') return;
		path = dir;
		await loadPreview();
	}

	async function loadPreview() {
		reading = true;
		error = '';
		result = null;
		try {
			const p = await api.pfPreview(path);
			preview = p;
			picked = Object.fromEntries(p.playlists.map((pl) => [pl.id, true]));
			accountMap = Object.fromEntries(p.accounts.map((a) => [a.id, a.forge_account]));
			tokens = false;
		} catch (e) {
			error = errorText(e);
			if (String(e) === 'pf_running') runDetect();
		} finally {
			reading = false;
		}
	}

	const forgeById = $derived(new Map((preview?.forge_accounts ?? []).map((f) => [f.id, f])));

	type Where = 'index' | 'history' | 'missing' | 'deleted';
	function where(pl: api.PfPreviewPlaylist): Where {
		if (pl.deleted_remotely) return 'deleted';
		const forge = accountMap[pl.account_id];
		const account = forge ? forgeById.get(forge) : undefined;
		if (!account) return 'missing';
		return account.active ? 'index' : 'history';
	}

	function targetText(pl: api.PfPreviewPlaylist): string {
		switch (where(pl)) {
			case 'index':
				return !pl.in_forge
					? t('pf_import.target_index')
					: pl.winner === 'forge'
						? t('pf_import.target_index_kept')
						: t('pf_import.target_index_replaces');
			case 'history':
				return t('pf_import.target_history');
			case 'deleted':
				return t('pf_import.target_deleted');
			default:
				return t('pf_import.target_missing');
		}
	}

	const selectedIds = $derived(Object.keys(picked).filter((id) => picked[id]));
	const anyMissing = $derived(
		(preview?.playlists ?? []).some((pl) => picked[pl.id] && ['missing', 'deleted'].includes(where(pl)))
	);
	const tokenChannels = $derived((preview?.accounts ?? []).filter((a) => a.data_api).map((a) => a.id));
	const offerTokens = $derived(
		!!preview && dataApi && preview.pf_client_secret && tokenChannels.length > 0
	);
	const running = $derived(detect?.running === true);

	function setAll(on: boolean) {
		picked = Object.fromEntries((preview?.playlists ?? []).map((pl) => [pl.id, on]));
	}

	function accountLabel(f: api.PfForgeAccount): string {
		const name = f.name ?? t('pf_import.account_unnamed');
		return f.active ? t('pf_import.account_active', { name }) : name;
	}

	async function runImport() {
		if (!preview) return;
		importing = true;
		error = '';
		progress = null;
		tokenOutcomes = [];
		try {
			const r = await api.pfImportApply({
				path,
				playlists: selectedIds,
				account_map: accountMap,
				missing_as: missingAs,
				alerts,
				downloads,
				settings,
				appearance,
				data_api: dataApi
			});
			if (tokens && offerTokens && preview.tokens_compatible) {
				try {
					tokenOutcomes = await api.pfImportCredentials(tokenChannels, true, path);
				} catch (e) {
					r.report.warnings.push(errorText(e));
				}
			}
			const theme = r.report.theme;
			if (theme) {
				applyTheme(theme.id as ThemeId);
				if (theme.mode) setMode(theme.mode);
			}
			if (r.report.locale) await setLocale(r.report.locale as LocaleId).catch(() => {});
			if (r.report.local_created > 0) refreshLocalPlaylists();
			result = r;
			runDetect();
		} catch (e) {
			error = errorText(e);
			if (String(e) === 'pf_running') runDetect();
		} finally {
			importing = false;
		}
	}

	function showImport() {
		ui.settingsOpen = false;
		imp.open = true;
	}

	async function removeTask() {
		taskBusy = true;
		taskError = '';
		try {
			await api.pfUnregisterTask(true);
			taskDone = true;
			confirmTask = false;
		} catch (e) {
			taskError = errorText(e);
		} finally {
			taskBusy = false;
		}
	}

	function stepText(p: api.PfProgress): string {
		const key = `pf_import.step_${p.step}` as TranslationKey;
		const text = t(key, { done: p.done + 1, total: p.total });
		return text === key ? p.step : text;
	}

	const lines = $derived.by(() => {
		if (!result) return [] as string[];
		const r = result.report;
		const out: string[] = [];
		const add = (n: number, key: TranslationKey, extra: Record<string, number> = {}) => {
			if (n > 0) out.push(t(key, { count: n, ...extra }));
		};
		add(r.playlists_indexed, 'pf_import.r_indexed');
		add(r.playlists_kept, 'pf_import.r_kept');
		add(r.playlists_history, 'pf_import.r_history');
		add(r.local_created, 'pf_import.r_local');
		add(r.local_existing, 'pf_import.r_local_existing');
		if (result.import_started) add(r.account_queued, 'pf_import.r_account');
		add(r.snapshots, 'pf_import.r_snapshots');
		add(r.alerts, 'pf_import.r_alerts');
		add(r.downloads, 'pf_import.r_downloads', { missing: r.downloads_missing });
		add(r.settings, 'pf_import.r_settings');
		add(r.settings_skipped.length, 'pf_import.r_settings_skipped');
		add(r.jobs, 'pf_import.r_jobs');
		add(r.quota_entries, 'pf_import.r_quota');
		add(r.ytdata_accounts, 'pf_import.r_ytdata');
		if (r.client_secret_copied) out.push(t('pf_import.r_client'));
		add(tokenOutcomes.filter((o) => o.outcome === 'imported').length, 'pf_import.r_tokens');
		add(tokenOutcomes.filter((o) => o.outcome === 'error').length, 'pf_import.r_tokens_failed');
		return out;
	});
</script>

<div class={CARD}>
	{#if detecting && !detect}
		<div class={ROW}><p class={HINT}>{t('pf_import.detecting')}</p></div>
	{:else}
		<div class="{ROW} flex flex-col gap-2">
			{#if detect?.found || path}
				<p class="text-sm font-medium">
					{t('pf_import.found', { path: path ?? detect?.path ?? '' })}{#if detect?.user_version && !path}
						· {t('pf_import.version', { version: detect?.user_version ?? '' })}{/if}
				</p>
			{:else}
				<p class={HINT}>{t('pf_import.not_found')}</p>
			{/if}
			{#if detect?.error && !path}
				<p class="text-xs text-destructive">{errorText(detect.error)}</p>
			{/if}
			{#if running}
				<Alert>
					<HugeiconsIcon icon={Alert02Icon} size={16} strokeWidth={1.8} />
					<AlertDescription>
						<p>{t('pf_import.running')}</p>
						<Button class="mt-2" size="sm" variant="outline" onclick={runDetect} disabled={detecting}>
							{t('pf_import.check_again')}
						</Button>
					</AlertDescription>
				</Alert>
			{/if}
			<div class="flex flex-wrap items-center gap-2">
				{#if detect?.found || path}
					<Button
						size="sm"
						onclick={loadPreview}
						disabled={reading || importing || running || detect?.too_new === true}
					>
						{reading ? t('pf_import.reading') : t('pf_import.preview')}
					</Button>
				{/if}
				<Button size="sm" variant="outline" onclick={chooseFolder} disabled={reading || importing}>
					{t('pf_import.choose_folder')}
				</Button>
			</div>
			{#if error}
				<p class="text-xs text-destructive">{error}</p>
			{/if}
		</div>

		{#if preview && !result}
			{@const s = preview.summary}
			<div class={ROW}>
				<p class={HINT}>
					{t('pf_import.summary', {
						playlists: s.playlists,
						items: s.items,
						snapshots: s.snapshots,
						alerts: s.alerts,
						downloads: s.downloads,
						jobs: s.jobs_pending
					})}
				</p>
			</div>

			{#if preview.accounts.length}
				<div class="{ROW} flex flex-col gap-2">
					<p class="text-sm font-medium">{t('pf_import.accounts_title')}</p>
					<p class={HINT}>{t('pf_import.accounts_hint')}</p>
					{#each preview.accounts as a (a.id)}
						<label class="flex items-center justify-between gap-3 text-sm">
							<span class="truncate">{a.title}</span>
							<select
								class="h-8 max-w-56 rounded-md border border-input bg-input/30 px-2 text-sm"
								value={accountMap[a.id] ?? ''}
								onchange={(e) => (accountMap[a.id] = e.currentTarget.value || null)}
								disabled={importing}
							>
								<option value="">{t('pf_import.account_none')}</option>
								{#each preview.forge_accounts as f (f.id)}
									<option value={f.id}>{accountLabel(f)}</option>
								{/each}
							</select>
						</label>
					{/each}
				</div>
			{/if}

			<div class="{ROW} flex flex-col gap-2">
				<div class="flex items-center justify-between">
					<p class="text-sm font-medium">{t('pf_import.playlists_title')}</p>
					<div class="flex gap-1">
						<Button size="sm" variant="ghost" onclick={() => setAll(true)}>{t('pf_import.select_all')}</Button>
						<Button size="sm" variant="ghost" onclick={() => setAll(false)}>{t('pf_import.select_none')}</Button>
					</div>
				</div>
				<div class="flex max-h-64 flex-col gap-1.5 overflow-y-auto pr-1">
					{#each preview.playlists as pl (pl.id)}
						<label class="flex cursor-pointer items-start gap-2 text-sm">
							<input type="checkbox" class="{CHECK} mt-0.5" bind:checked={picked[pl.id]} disabled={importing} />
							<span class="min-w-0 flex-1">
								<span class="block truncate">{pl.title}</span>
								<span
									class="block text-[11px] {where(pl) === 'index' && pl.in_forge
										? 'text-amber-600 dark:text-amber-400'
										: 'text-muted-foreground'}"
								>
									{t('pf_import.items', { count: pl.items })} · {pl.privacy} · {targetText(pl)}
								</span>
							</span>
						</label>
					{/each}
				</div>
			</div>

			{#if anyMissing}
				<div class="{ROW} flex flex-col gap-1.5">
					<p class="text-sm font-medium">{t('pf_import.missing_title')}</p>
					{#each [['local', 'pf_import.missing_local'], ['account', 'pf_import.missing_account'], ['skip', 'pf_import.missing_skip']] as [value, key] (value)}
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<input
								type="radio"
								name="pf-missing"
								class={CHECK}
								{value}
								checked={missingAs === value}
								onchange={() => (missingAs = value as api.PfMissingAs)}
								disabled={importing}
							/>
							{t(key as TranslationKey)}
						</label>
					{/each}
				</div>
			{/if}

			<div class="{ROW} flex flex-col gap-1.5">
				<p class="text-sm font-medium">{t('pf_import.include_title')}</p>
				<label class="flex cursor-pointer items-center gap-2 text-sm">
					<input type="checkbox" class={CHECK} bind:checked={alerts} disabled={importing} />
					{t('pf_import.include_alerts')}
				</label>
				<label class="flex cursor-pointer items-center gap-2 text-sm">
					<input type="checkbox" class={CHECK} bind:checked={downloads} disabled={importing} />
					{t('pf_import.include_downloads')}
				</label>
				<label class="flex cursor-pointer items-center gap-2 text-sm">
					<input type="checkbox" class={CHECK} bind:checked={settings} disabled={importing} />
					{t('pf_import.include_settings')}
				</label>
				<label class="flex cursor-pointer items-center gap-2 text-sm">
					<input type="checkbox" class={CHECK} bind:checked={appearance} disabled={importing} />
					{t('pf_import.include_appearance')}
				</label>
				<label class="flex cursor-pointer items-center gap-2 text-sm">
					<input type="checkbox" class={CHECK} bind:checked={dataApi} disabled={importing} />
					{t('pf_import.include_data_api')}
				</label>
			</div>

			{#if offerTokens}
				<div class="{ROW} flex flex-col gap-1.5">
					{#if preview.tokens_compatible}
						<label class="flex cursor-pointer items-center gap-2 text-sm font-medium">
							<input type="checkbox" class={CHECK} bind:checked={tokens} disabled={importing} />
							{t('pf_import.tokens_label')}
						</label>
						<p class={HINT}>{t('pf_import.tokens_hint')}</p>
					{:else}
						<p class={HINT}>{t('pf_import.tokens_incompatible')}</p>
					{/if}
				</div>
			{/if}

			<div class="{ROW} flex flex-col gap-2">
				<div class="flex items-center gap-3">
					<Button
						size="sm"
						onclick={runImport}
						disabled={importing || running || (selectedIds.length === 0 && !dataApi && !settings)}
					>
						{importing ? t('pf_import.importing') : t('pf_import.import')}
					</Button>
					{#if importing && progress}
						<span class={HINT}>{stepText(progress)}</span>
					{/if}
				</div>
			</div>
		{/if}

		{#if result}
			<div class="{ROW} flex flex-col gap-2">
				<p class="flex items-center gap-2 text-sm font-medium">
					<HugeiconsIcon icon={DatabaseImportIcon} size={16} strokeWidth={1.8} />
					{t('pf_import.done_title')}
				</p>
				<ul class="list-disc pl-5 text-xs leading-relaxed text-muted-foreground">
					{#each lines as line, i (i)}
						<li>{line}</li>
					{/each}
				</ul>
				{#if result.import_error}
					<p class="text-xs text-destructive">
						{t('pf_import.account_import_failed', { error: errorText(result.import_error) })}
					</p>
				{/if}
				{#if result.import_started}
					<div>
						<Button size="sm" variant="outline" onclick={showImport}>{t('pf_import.show_import')}</Button>
					</div>
				{/if}
				<div>
					<Button size="sm" variant="ghost" onclick={loadPreview} disabled={running}>{t('pf_import.again')}</Button>
				</div>
			</div>

			{#if detect?.task || taskDone}
				<div class="{ROW} flex flex-col gap-2">
					<p class="text-sm font-medium">{t('pf_import.task_title')}</p>
					{#if taskDone}
						<p class={HINT}>{t('pf_import.task_removed')}</p>
					{:else}
						<p class={HINT}>{t('pf_import.task_hint')}</p>
						{#if confirmTask}
							<p class="text-xs">{t('pf_import.task_confirm')}</p>
							<div class="flex gap-2">
								<Button size="sm" variant="destructive" onclick={removeTask} disabled={taskBusy}>
									{t('pf_import.task_confirm_yes')}
								</Button>
								<Button size="sm" variant="outline" onclick={() => (confirmTask = false)} disabled={taskBusy}>
									{t('pf_import.task_cancel')}
								</Button>
							</div>
						{:else}
							<div>
								<Button size="sm" variant="outline" onclick={() => (confirmTask = true)}>
									{t('pf_import.task_remove')}
								</Button>
							</div>
						{/if}
						{#if taskError}
							<p class="text-xs text-destructive">{taskError}</p>
						{/if}
					{/if}
				</div>
			{/if}
		{/if}
	{/if}
</div>
