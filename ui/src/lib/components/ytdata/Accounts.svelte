<script lang="ts">
	// Settings ▸ YouTube Data API: the channels connected through OAuth. Connect opens Google's consent
	// page in the system browser (Rust waits on a loopback port for it, up to five minutes, and can be
	// told to stop); the result comes back as `ytdata-connect-finished`. Each channel acts for one
	// cookie account (`linked_account`); a new one is linked to the account signed in now when that
	// one has none. Disconnect revokes and forgets the sign-in; the channel's queued jobs stay,
	// unowned. Nothing here ever holds a token. Notes are inline: a toast renders behind the modal.
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Copy01Icon, Loading03Icon, PlusSignIcon, YoutubeIcon } from '@hugeicons/core-free-icons';
	import { Button } from '$lib/components/ui/button';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import * as Select from '$lib/components/ui/select';
	import * as api from '$lib/api';
	import { copyText } from '$lib/clipboard';
	import { t } from '$lib/i18n.svelte';
	import { refreshYtData } from '$lib/ytdata.svelte';

	let { secret }: { secret: api.ClientSecretInfo | null } = $props();

	let accounts = $state<api.YtDataAccount[]>([]);
	let cookieAccounts = $state<api.SavedAccount[]>([]);
	/** The attempt in progress and its consent URL (to copy when no browser opened). */
	let connecting = $state<{ attempt: number; url: string } | null>(null);
	let note = $state<{ message: string; error: boolean } | null>(null);
	let removing = $state<api.YtDataAccount | null>(null);
	let confirmOpen = $state(false);

	const NO_LINK = '__none__';

	async function reload() {
		try {
			[accounts, cookieAccounts] = await Promise.all([api.ytdataAccounts(), api.getGoogleAccounts()]);
		} catch (e) {
			note = { message: String(e), error: true };
		}
	}

	onMount(() => {
		void reload();
		const off = api.onYtDataConnectFinished((r) => {
			if (!connecting || r.attempt !== connecting.attempt) return;
			connecting = null;
			if (r.ok) {
				note = { message: t('ytdata.accounts.connected', { title: r.title ?? '' }), error: false };
			} else if (r.code === 'timed_out') {
				note = { message: t('ytdata.accounts.timed_out'), error: true };
			} else if (r.code === 'failed') {
				note = { message: t('ytdata.accounts.connect_failed', { error: r.error ?? '' }), error: true };
			}
			void reload();
			void refreshYtData();
		});
		return () => {
			void off.then((f) => f()).catch(() => {});
			// Leaving the tab mid-wait gives up on it: nothing would be left to show its end.
			if (connecting) void api.ytdataConnectCancel().catch(() => {});
		};
	});

	async function connect() {
		note = null;
		try {
			const started = await api.ytdataConnectStart();
			connecting = { attempt: started.attempt, url: started.authorize_url };
		} catch (e) {
			note = { message: String(e), error: true };
		}
	}

	async function cancel() {
		try {
			await api.ytdataConnectCancel();
		} catch {
			// The wait ends on its own anyway.
		}
		connecting = null;
	}

	async function copyLink() {
		if (!connecting) return;
		try {
			await copyText(connecting.url);
			note = { message: t('ytdata.accounts.link_copied'), error: false };
		} catch (e) {
			note = { message: String(e), error: true };
		}
	}

	function askDisconnect(account: api.YtDataAccount) {
		removing = account;
		confirmOpen = true;
	}

	async function disconnect() {
		const account = removing;
		confirmOpen = false;
		if (!account) return;
		try {
			await api.ytdataDisconnect(account.channel_id);
			note = { message: t('ytdata.accounts.disconnected', { title: account.title }), error: false };
		} catch (e) {
			note = { message: String(e), error: true };
		}
		await reload();
		await refreshYtData();
	}

	async function link(account: api.YtDataAccount, value: string) {
		const target = value === NO_LINK ? null : value;
		try {
			await api.ytdataLinkAccount(account.channel_id, target);
		} catch (e) {
			note = { message: String(e), error: true };
		}
		await reload();
		await refreshYtData();
	}

	function cookieLabel(id: string | null): string {
		if (!id) return t('ytdata.accounts.link_none');
		const a = cookieAccounts.find((c) => c.id === id);
		if (!a) return t('ytdata.accounts.unknown_account');
		return a.name || a.email || a.handle || id;
	}
</script>

<div class="divide-y divide-border/60 overflow-hidden rounded-xl border bg-card">
	{#each accounts as account (account.channel_id)}
		<div class="flex items-center gap-3 px-4 py-3">
			{#if account.thumb}
				<img src={account.thumb} alt="" class="h-9 w-9 shrink-0 rounded-full object-cover" />
			{:else}
				<span class="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-muted">
					<HugeiconsIcon icon={YoutubeIcon} class="h-4 w-4 text-muted-foreground" />
				</span>
			{/if}
			<div class="min-w-0 flex-1">
				<p class="truncate text-sm font-medium">{account.title}</p>
				<p class="text-xs {account.status === 'connected' ? 'text-muted-foreground' : 'text-amber-500'}">
					{t(`ytdata.accounts.status_${account.status}`)}
				</p>
			</div>
			<Select.Root
				type="single"
				value={account.linked_account ?? NO_LINK}
				onValueChange={(v) => link(account, v)}
			>
				<Select.Trigger class="w-40 shrink-0" aria-label={t('ytdata.accounts.linked_to')} title={t('ytdata.accounts.link_hint')}>
					<span class="flex-1 truncate text-left">{cookieLabel(account.linked_account)}</span>
				</Select.Trigger>
				<Select.Content>
					<Select.Item value={NO_LINK} label={t('ytdata.accounts.link_none')}>
						{t('ytdata.accounts.link_none')}
					</Select.Item>
					{#each cookieAccounts as c (c.id)}
						<Select.Item value={c.id} label={cookieLabel(c.id)}>{cookieLabel(c.id)}</Select.Item>
					{/each}
				</Select.Content>
			</Select.Root>
			{#if account.status === 'reauth_required'}
				<Button size="sm" class="shrink-0" disabled={!secret || !!connecting} onclick={connect}>
					{t('ytdata.accounts.reconnect')}
				</Button>
			{/if}
			<Button variant="ghost" size="sm" class="shrink-0" onclick={() => askDisconnect(account)}>
				{t('ytdata.accounts.disconnect')}
			</Button>
		</div>
	{:else}
		<p class="px-4 py-3.5 text-sm text-muted-foreground">{t('ytdata.accounts.none')}</p>
	{/each}

	<div class="px-4 py-3.5">
		{#if connecting}
			<div class="flex items-start gap-3">
				<HugeiconsIcon icon={Loading03Icon} class="mt-0.5 h-4 w-4 shrink-0 animate-spin text-primary" />
				<div class="min-w-0 flex-1">
					<p class="text-sm font-medium">{t('ytdata.accounts.connecting')}</p>
					<p class="mt-0.5 text-xs leading-relaxed text-muted-foreground">{t('ytdata.accounts.connecting_hint')}</p>
					<div class="mt-2 flex gap-2">
						<Button variant="outline" size="sm" class="gap-1.5" onclick={copyLink}>
							<HugeiconsIcon icon={Copy01Icon} class="h-3.5 w-3.5" />
							{t('ytdata.accounts.copy_link')}
						</Button>
						<Button variant="ghost" size="sm" onclick={cancel}>{t('ytdata.accounts.cancel')}</Button>
					</div>
				</div>
			</div>
		{:else}
			<div class="flex items-center justify-between gap-6">
				<p class="max-w-prose text-xs leading-relaxed text-muted-foreground">
					{secret ? t('ytdata.accounts.connect_hint') : t('ytdata.accounts.needs_secret')}
				</p>
				<Button size="sm" class="shrink-0 gap-1.5" disabled={!secret} onclick={connect}>
					<HugeiconsIcon icon={PlusSignIcon} class="h-4 w-4" />
					{accounts.length ? t('ytdata.accounts.connect_another') : t('ytdata.accounts.connect')}
				</Button>
			</div>
		{/if}
		{#if note}
			<p class="mt-2 text-xs {note.error ? 'text-destructive' : 'text-muted-foreground'}" role="status">
				{note.message}
			</p>
		{/if}
	</div>
</div>

<AlertDialog.Root bind:open={confirmOpen}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>
				{t('ytdata.accounts.disconnect_title', { title: removing?.title ?? '' })}
			</AlertDialog.Title>
			<AlertDialog.Description>{t('ytdata.accounts.disconnect_desc')}</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>{t('common.cancel')}</AlertDialog.Cancel>
			<AlertDialog.Action onclick={disconnect}>{t('ytdata.accounts.disconnect')}</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
