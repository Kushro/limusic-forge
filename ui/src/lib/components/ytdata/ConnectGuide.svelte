<script lang="ts">
	// Settings ▸ YouTube Data API, first part: the OAuth client the app talks to Google with, and the
	// step-by-step guide to making one (PlaylistForge's onboarding wizard, as a list). The user brings
	// their own Google Cloud project: the app ships no client id or secret. Links open in the system
	// browser (`open_external`), never in the app's webview. The imported file is validated and
	// copied by Rust; only its masked client id comes back.
	import { untrack } from 'svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		Alert02Icon,
		CheckmarkCircle02Icon,
		FileImportIcon,
		LinkSquare02Icon
	} from '@hugeicons/core-free-icons';
	import { Button } from '$lib/components/ui/button';
	import * as api from '$lib/api';
	import { t, type TranslationKey } from '$lib/i18n.svelte';
	import { refreshYtData } from '$lib/ytdata.svelte';

	let {
		secret = $bindable(null)
	}: {
		/** The imported client, or null; the tab reads it and passes it to the channels list too. */
		secret: api.ClientSecretInfo | null;
	} = $props();

	type Step = {
		n: number;
		title: TranslationKey;
		desc: TranslationKey;
		link?: { url: string; label: TranslationKey };
	};
	const STEPS: Step[] = [
		{
			n: 1,
			title: 'ytdata.guide.step1_title',
			desc: 'ytdata.guide.step1_desc',
			link: { url: 'https://console.cloud.google.com/projectcreate', label: 'ytdata.guide.step1_link' }
		},
		{
			n: 2,
			title: 'ytdata.guide.step2_title',
			desc: 'ytdata.guide.step2_desc',
			link: {
				url: 'https://console.cloud.google.com/apis/library/youtube.googleapis.com',
				label: 'ytdata.guide.step2_link'
			}
		},
		{
			n: 3,
			title: 'ytdata.guide.step3_title',
			desc: 'ytdata.guide.step3_desc',
			link: {
				url: 'https://console.cloud.google.com/apis/credentials/consent',
				label: 'ytdata.guide.step3_link'
			}
		},
		{
			n: 4,
			title: 'ytdata.guide.step4_title',
			desc: 'ytdata.guide.step4_desc',
			link: { url: 'https://console.cloud.google.com/apis/credentials', label: 'ytdata.guide.step4_link' }
		},
		{ n: 5, title: 'ytdata.guide.step5_title', desc: 'ytdata.guide.step5_desc' },
		{ n: 6, title: 'ytdata.guide.step6_title', desc: 'ytdata.guide.step6_desc' }
	];

	let importing = $state(false);
	let note = $state<{ message: string; error: boolean } | null>(null);
	// Open while no client is imported (as the tab mounts this); after that it is one click away.
	let guideOpen = $state(untrack(() => secret === null));

	async function openLink(url: string) {
		try {
			await api.openExternal(url);
		} catch (e) {
			note = { message: String(e), error: true };
		}
	}

	async function importSecret() {
		note = null;
		const picked = await open({
			title: t('ytdata.secret.picker_title'),
			filters: [{ name: t('ytdata.secret.filter'), extensions: ['json'] }]
		});
		if (typeof picked !== 'string') return;
		importing = true;
		try {
			secret = await api.ytdataImportClientSecret(picked);
			note = { message: t('ytdata.secret.done'), error: false };
			await refreshYtData();
		} catch (e) {
			note = { message: String(e), error: true };
		} finally {
			importing = false;
		}
	}
</script>

<div class="divide-y divide-border/60 overflow-hidden rounded-xl border bg-card">
	<div class="px-4 py-3.5">
		<div class="flex items-start justify-between gap-6">
			<div class="min-w-0">
				<span class="text-sm font-medium">{t('ytdata.secret.title')}</span>
				{#if secret}
					<p class="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
						<HugeiconsIcon icon={CheckmarkCircle02Icon} class="h-3.5 w-3.5 shrink-0 text-primary" />
						<span class="truncate font-mono">{t('ytdata.secret.imported', { id: secret.masked_client_id })}</span>
					</p>
					<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
						{t('ytdata.secret.replace_hint')}
					</p>
				{:else}
					<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
						{t('ytdata.secret.none')}
					</p>
				{/if}
			</div>
			<Button
				variant={secret ? 'outline' : 'default'}
				size="sm"
				class="shrink-0 gap-1.5"
				disabled={importing}
				onclick={importSecret}
			>
				<HugeiconsIcon icon={FileImportIcon} class="h-4 w-4" />
				{secret ? t('ytdata.secret.replace') : t('ytdata.secret.import')}
			</Button>
		</div>
		{#if note}
			<p class="mt-2 text-xs {note.error ? 'text-destructive' : 'text-muted-foreground'}" role="status">
				{note.message}
			</p>
		{/if}
	</div>

	<details class="group px-4 py-3.5" bind:open={guideOpen}>
		<summary class="cursor-pointer select-none text-sm font-medium">
			{t('ytdata.guide.title')}
		</summary>
		<p class="mt-2 max-w-prose text-xs leading-relaxed text-muted-foreground">{t('ytdata.guide.intro')}</p>
		<ol class="mt-3 flex flex-col gap-3">
			{#each STEPS as step (step.n)}
				<li class="flex gap-3">
					<span
						class="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-primary/12 text-xs font-semibold text-primary"
						aria-hidden="true"
					>
						{step.n}
					</span>
					<div class="min-w-0 flex-1">
						<p class="text-sm font-medium">{t(step.title)}</p>
						<p class="mt-0.5 max-w-prose whitespace-pre-line text-xs leading-relaxed text-muted-foreground">
							{t(step.desc)}
						</p>
						{#if step.n === 3}
							<div class="mt-2 flex gap-2 rounded-lg border border-amber-500/30 bg-amber-500/5 p-2.5 text-xs">
								<HugeiconsIcon icon={Alert02Icon} class="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-500" />
								<div>
									<p class="font-medium">{t('ytdata.guide.step3_warning_title')}</p>
									<p class="mt-0.5 leading-relaxed text-muted-foreground">{t('ytdata.guide.step3_warning_body')}</p>
								</div>
							</div>
						{/if}
						{#if step.link}
							{@const link = step.link}
							<Button variant="link" size="sm" class="mt-1 h-auto gap-1 px-0" onclick={() => openLink(link.url)}>
								{t(link.label)}
								<HugeiconsIcon icon={LinkSquare02Icon} class="h-3.5 w-3.5" />
							</Button>
						{/if}
						{#if step.n === 5 && !secret}
							<Button size="sm" class="mt-2 gap-1.5" disabled={importing} onclick={importSecret}>
								<HugeiconsIcon icon={FileImportIcon} class="h-4 w-4" />
								{t('ytdata.secret.import')}
							</Button>
						{/if}
					</div>
				</li>
			{/each}
		</ol>
	</details>
</div>
