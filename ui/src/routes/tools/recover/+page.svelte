<script lang="ts">
	// Tools ▸ Recover tracks (F4): an assistant over every playlist at once. Each step is its own
	// component under `components/recover/`, mounted for `rec.step`; they all read and write the
	// session store (`recover.svelte.ts`), so leaving the page halfway and coming back resumes it.
	//   pick    RecoverPick: choose the tracks (`rec.selected`, in order via `selectedKeys()`)
	//   titles  recover the missing titles (`recoverTitles`, `onRecoverProgress` → `rec.snapshot`)
	//   search  find replacements (`recoverSearch`, same progress)
	//   review  approve or change each row's pick (`recoverRows`, `recoverPick`)
	//   apply   replace or append (`recoverApply`)
	// A step moves on with `goToStep(nextStep())` and back with `goToStep(previous)`.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ArrowLeft01Icon, DataRecoveryIcon, RefreshIcon } from '@hugeicons/core-free-icons';
	import { goToStep, nextStep, rec, RECOVER_STEPS, resetRecover, type RecoverStep } from '$lib/recover.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Button } from '$lib/components/ui/button';
	import RecoverPick from '$lib/components/recover/RecoverPick.svelte';
	import RecoverProgress from '$lib/components/recover/RecoverProgress.svelte';
	import RecoverReview from '$lib/components/recover/RecoverReview.svelte';
	import RecoverApply from '$lib/components/recover/RecoverApply.svelte';

	const current = $derived(RECOVER_STEPS.indexOf(rec.step));
	const previous = $derived<RecoverStep>(RECOVER_STEPS[Math.max(current - 1, 0)]);
</script>

<div class="p-6">
	<div class="mb-4 flex flex-wrap items-start gap-3">
		<div class="min-w-0 flex-1">
			<a href="/tools" class="mb-1 inline-flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground">
				<HugeiconsIcon icon={ArrowLeft01Icon} class="h-3 w-3" />
				{t('tools.hub_title')}
			</a>
			<h1 class="flex items-center gap-2 font-heading text-2xl font-bold tracking-tight">
				<HugeiconsIcon icon={DataRecoveryIcon} class="h-6 w-6 text-primary" />
				{t('recover.title')}
			</h1>
			<p class="mt-1 max-w-2xl text-sm text-muted-foreground">{t('recover.intro')}</p>
		</div>
		<Button variant="ghost" size="sm" onclick={resetRecover} disabled={rec.step === 'pick' && !rec.selected.size}>
			<HugeiconsIcon icon={RefreshIcon} class="h-4 w-4" />
			{t('recover.start_over')}
		</Button>
	</div>

	<!-- The steps: done ones can be gone back to, later ones only through each step's own button. -->
	<ol class="mb-5 flex flex-wrap items-center gap-2" aria-label={t('recover.steps_label')}>
		{#each RECOVER_STEPS as s, i (s)}
			<li class="flex items-center gap-2">
				{#if i > 0}<span class="h-px w-6 bg-border"></span>{/if}
				<button
					type="button"
					class="flex items-center gap-2 rounded-full px-2 py-1 text-sm transition-colors disabled:cursor-default {i === current
						? 'font-semibold text-foreground'
						: i < current
							? 'text-primary hover:bg-primary/10'
							: 'text-muted-foreground'}"
					aria-current={i === current ? 'step' : undefined}
					disabled={i >= current}
					onclick={() => goToStep(s)}
				>
					<span
						class="flex h-6 w-6 items-center justify-center rounded-full text-xs tabular-nums {i === current
							? 'bg-primary text-primary-foreground'
							: i < current
								? 'bg-primary/15 text-primary'
								: 'bg-muted text-muted-foreground'}"
					>
						{i + 1}
					</span>
					{t(`recover.step_${s}`)}
				</button>
			</li>
		{/each}
	</ol>

	{#if rec.step === 'pick'}
		<RecoverPick onnext={() => goToStep(nextStep())} />
	{:else if rec.step === 'titles' || rec.step === 'search'}
		<!-- One instance per step: each reads its own rows and run on mount. -->
		{#key rec.step}
			<RecoverProgress onnext={() => goToStep(nextStep())} onback={() => goToStep(previous)} />
		{/key}
	{:else if rec.step === 'review'}
		<RecoverReview onnext={() => goToStep(nextStep())} onback={() => goToStep(previous)} />
	{:else}
		<RecoverApply onback={() => goToStep(previous)} />
	{/if}
</div>
