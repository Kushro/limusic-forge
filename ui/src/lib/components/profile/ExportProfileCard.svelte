<script lang="ts">
	// Settings ▸ Import & migrate ▸ Profile ▸ export. Rust builds the file (src-tauri/src/profile/);
	// this gathers the choices and the webview's localStorage. The passphrase only lives in this
	// component's state for the call and is cleared right after.
	import { save } from '@tauri-apps/plugin-dialog';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Alert02Icon, CheckmarkCircle02Icon } from '@hugeicons/core-free-icons';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Alert, AlertDescription } from '$lib/components/ui/alert';
	import * as api from '$lib/api';
	import { t } from '$lib/i18n.svelte';
	import { collectWebStorage } from '$lib/profile-webstorage';
	import { profileErrorText } from './profile-errors';

	const CARD = 'divide-y divide-border/60 overflow-hidden rounded-xl border bg-card';
	const ROW = 'px-4 py-3';
	const HINT = 'text-xs leading-relaxed text-muted-foreground';
	const CHECK = 'size-4 shrink-0 accent-primary';
	const MIN_PASSPHRASE = 10;

	let withSecrets = $state(false);
	let withCaches = $state(false);
	let pass = $state('');
	let pass2 = $state('');
	let busy = $state(false);
	let error = $state('');
	let report = $state<api.ProfileExportReport | null>(null);

	// Chars, not UTF-16 units: the same count Rust uses.
	const passLen = $derived([...pass].length);
	const passShort = $derived(pass.length > 0 && passLen < MIN_PASSPHRASE);
	const passMismatch = $derived(pass2.length > 0 && pass !== pass2);
	const passOk = $derived(passLen >= MIN_PASSPHRASE && pass === pass2);
	const canExport = $derived(!busy && (!withSecrets || passOk));

	const sizeLabel = (bytes: number) =>
		bytes >= 1 << 30
			? `${(bytes / (1 << 30)).toFixed(1)} GB`
			: bytes >= 1 << 20
				? `${(bytes / (1 << 20)).toFixed(1)} MB`
				: `${Math.max(1, Math.round(bytes / 1024))} KB`;

	async function run() {
		error = '';
		report = null;
		let dest: string | null = null;
		try {
			const name = await api.profileDefaultFileName().catch(() => 'LiMusicForge.lmfprofile');
			dest = await save({
				defaultPath: name,
				filters: [{ name: t('settings.profile.file_filter'), extensions: ['lmfprofile'] }]
			});
		} catch (e) {
			error = String(e);
			return;
		}
		if (!dest) return;
		busy = true;
		try {
			report = await api.profileExport(
				dest,
				withSecrets,
				withCaches,
				withSecrets ? pass : null,
				collectWebStorage()
			);
			pass = '';
			pass2 = '';
		} catch (e) {
			error = profileErrorText(e);
		} finally {
			busy = false;
		}
	}
</script>

<div class={CARD}>
	<div class="{ROW} space-y-1">
		<p class="text-sm font-medium">{t('settings.profile.export_title')}</p>
		<p class={HINT}>{t('settings.profile.export_hint')}</p>
	</div>

	<div class="{ROW} space-y-1">
		<label class="flex items-center gap-2 text-sm">
			<input type="checkbox" class={CHECK} bind:checked={withCaches} disabled={busy} />
			{t('settings.profile.include_caches')}
		</label>
		<p class={HINT}>{t('settings.profile.include_caches_hint')}</p>
	</div>

	<div class="{ROW} space-y-2">
		<label class="flex items-center gap-2 text-sm">
			<input type="checkbox" class={CHECK} bind:checked={withSecrets} disabled={busy} />
			{t('settings.profile.include_secrets')}
		</label>
		<p class={HINT}>{t('settings.profile.include_secrets_hint')}</p>
		{#if withSecrets}
			<Alert variant="destructive">
				<HugeiconsIcon icon={Alert02Icon} size={16} strokeWidth={1.8} />
				<AlertDescription>
					<p class="font-medium">{t('settings.profile.secrets_warning_title')}</p>
					<p>{t('settings.profile.secrets_warning')}</p>
				</AlertDescription>
			</Alert>
			<div class="grid gap-2 sm:grid-cols-2">
				<Input
					type="password"
					autocomplete="new-password"
					placeholder={t('settings.profile.passphrase')}
					aria-label={t('settings.profile.passphrase')}
					bind:value={pass}
					disabled={busy}
				/>
				<Input
					type="password"
					autocomplete="new-password"
					placeholder={t('settings.profile.passphrase_confirm')}
					aria-label={t('settings.profile.passphrase_confirm')}
					bind:value={pass2}
					disabled={busy}
				/>
			</div>
			{#if passShort}
				<p class="text-xs text-destructive">{t('settings.profile.errors.passphrase_too_short')}</p>
			{:else if passMismatch}
				<p class="text-xs text-destructive">{t('settings.profile.passphrase_mismatch')}</p>
			{/if}
			<p class={HINT}>{t('settings.profile.passphrase_hint')}</p>
		{/if}
	</div>

	<div class="{ROW} flex items-center gap-3">
		<Button size="sm" disabled={!canExport} onclick={run}>
			{busy ? t('settings.profile.exporting') : t('settings.profile.export_button')}
		</Button>
		{#if busy}<span class={HINT}>{t('settings.profile.exporting_hint')}</span>{/if}
	</div>

	{#if error}
		<div class={ROW}><p class="text-xs text-destructive">{error}</p></div>
	{/if}
	{#if report}
		<div class="{ROW} space-y-2">
			<p class="flex items-center gap-2 text-sm">
				<HugeiconsIcon icon={CheckmarkCircle02Icon} size={16} strokeWidth={1.8} />
				{t('settings.profile.export_done', { size: sizeLabel(report.bytes) })}
			</p>
			<p class="break-all text-[11px] text-muted-foreground">{report.path}</p>
			{#if report.token_failures.length > 0}
				<p class="text-xs text-destructive">
					{t('settings.profile.export_token_failures', { count: report.token_failures.length })}
				</p>
			{/if}
			{#if report.manifest.includes_secrets}
				<Alert>
					<HugeiconsIcon icon={Alert02Icon} size={16} strokeWidth={1.8} />
					<AlertDescription>
						<p class="font-medium">{t('settings.profile.shared_session_title')}</p>
						<p>{t('settings.profile.shared_session')}</p>
					</AlertDescription>
				</Alert>
			{/if}
		</div>
	{/if}
</div>
