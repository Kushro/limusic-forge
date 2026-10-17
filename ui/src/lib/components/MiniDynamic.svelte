<script lang="ts">
	// The dynamic mini player (`mini_mode = dynamic`): a 16:9 picture-in-picture window, the music
	// video edge to edge or the cover over a blurred copy of itself, with the controls on a layer
	// that only shows while the pointer is moving over it. The root layout picks this over
	// MiniPlayer when Rust says so (`window.__LIMUSIC_MINI__`, see $lib/minidyn); like MiniPlayer it
	// is the whole window and reads the same `playback` store.
	//
	// Rust owns the window's shape: it keeps 16:9 while resizing, remembers the size, and nothing
	// here ever asks for a size except the reset button. A track change only swaps what is painted.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		PreviousIcon,
		NextIcon,
		PlayIcon,
		PauseIcon,
		ThumbsUpIcon,
		ThumbsDownIcon,
		MusicNote01Icon,
		MaximizeScreenIcon,
		Mic01Icon,
		VolumeHighIcon,
		VolumeMute02Icon,
		ArrowShrink02Icon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import {
		playback,
		commitVolume,
		dragVolume,
		toggleMute,
		toggleNowPlayingRating,
		prefs,
		wheelVolume
	} from '$lib/player.svelte';
	import { thumb } from '$lib/thumb';
	import { claimVideo, hasVideo, parkVideo, showVideo } from '$lib/video.svelte';
	import { IDLE_MS, controlsVisible, miniConfig } from '$lib/minidyn';
	import VideoSurface from './VideoSurface.svelte';
	import LyricsView from './LyricsView.svelte';
	import Marquee from './Marquee.svelte';
	import { t } from '$lib/i18n.svelte';
	import { appearance } from '$lib/theme.svelte';

	// Read once: Rust wrote it before the page ran, and it only changes on the next open.
	const config = miniConfig();
	const side = config.lyrics === 'side';

	// Lyrics open or shut. Local, like the classic widget's tab: lyrics only fetch (and run the
	// karaoke clock) while LyricsView is mounted.
	let lyricsOpen = $state(false);

	const now = $derived(playback.now);
	// A local file has no YouTube identity, so there is nothing to like (see api.isLocalId).
	const likeable = $derived(!!now && !api.isLocalId(now.videoId));

	// Same rule as MiniPlayer: the music video in place of the cover only on top of the main
	// window's `music_videos`, and only once there is a picture to show.
	const miniVideo = $derived(prefs.musicVideos && prefs.miniVideo);
	const videoUp = $derived(miniVideo && showVideo());

	/** Borrow the picture into the window while there is one. Synchronous both ways, as
	 *  claimVideo/parkVideo require, and re-run whenever `hasVideo()` changes. */
	function holdVideo(box: HTMLElement) {
		if (!hasVideo()) return;
		claimVideo(box);
		return parkVideo;
	}

	// Clamped: a position tick can land past a stale duration.
	const progress = $derived(
		playback.duration ? Math.min(1, playback.position / playback.duration) : 0
	);

	// --- The control layer ---------------------------------------------------------------------
	// Up while the pointer moves over the window, down IDLE_MS after it stops or as soon as it
	// leaves. `clock` is the only thing that moves on its own, and only through one timeout that
	// lives as long as the last move does: no frame loop.
	let inside = $state(false);
	let lastMoveAt = $state(0);
	let clock = $state(0);
	let focusWithin = $state(false);
	let idleTimer: ReturnType<typeof setTimeout> | undefined;

	// Volume reveal and the two drags, as in MiniPlayer. A drag holds the layer up too: the pointer
	// can leave the window mid-drag, and the slider under it must not vanish.
	let volHover = $state(false);
	let volDragging = $state(false);
	let seekDragging = $state(false);
	const volOpen = $derived(volHover || volDragging);

	const shown = $derived(
		controlsVisible({
			lastMoveAt,
			now: clock,
			inside,
			dragging: volDragging || seekDragging,
			focusWithin
		})
	);

	function tick() {
		idleTimer = undefined;
		clock = performance.now();
		const left = lastMoveAt + IDLE_MS - clock;
		if (left > 0) idleTimer = setTimeout(tick, left);
	}
	function onMove() {
		inside = true;
		lastMoveAt = clock = performance.now();
		idleTimer ??= setTimeout(tick, IDLE_MS);
	}
	$effect(() => () => clearTimeout(idleTimer));

	// Keyboard focus only: a click focuses its button too, and that must not pin the layer up.
	function onFocusIn(e: FocusEvent) {
		focusWithin = e.target instanceof Element && e.target.matches(':focus-visible');
	}
	function onFocusOut(e: FocusEvent) {
		const root = e.currentTarget as HTMLElement;
		if (!(e.relatedTarget instanceof Node && root.contains(e.relatedTarget))) focusWithin = false;
	}

	function endDrags() {
		volDragging = false;
		seekDragging = false;
	}

	// The command destroys this very window, so its reply lands nowhere. Guarded because a double
	// click can arrive both as the second mousedown and as the dblclick that follows it.
	let leaving = false;
	function backToApp() {
		if (leaving) return;
		leaving = true;
		api.closeMini().catch(() => (leaving = false));
	}
	// Tauri's drag script answers the second mousedown of a double click on a drag region (by
	// asking to maximize, which this window refuses), and on Windows the drag it started on the
	// first one can swallow the dblclick event itself. The mousedown always arrives.
	// Only off the bars: the handle holds them, and a quick double click on next, seek, like or
	// volume is two presses of that control, not a way out. While the layer is down the bars let
	// the pointer through, so the whole picture answers again.
	function onBar(e: MouseEvent) {
		return e.target instanceof Element && !!e.target.closest('[data-mini-bar]');
	}
	function onDragDown(e: MouseEvent) {
		if (e.button === 0 && e.detail === 2 && !onBar(e)) backToApp();
	}
	function onDragDblClick(e: MouseEvent) {
		if (!onBar(e)) backToApp();
	}

	// Pop the thumb once when liking (not when clearing it), same as the player bar.
	let justLiked = $state(false);
	function toggleLike() {
		if (playback.rating !== 'like') justLiked = true;
		toggleNowPlayingRating();
	}

	// Seek: hold the dragged value locally so incoming position ticks can't yank the thumb out
	// from under the pointer; only invoke the seek on release.
	let seekDrag = $state<number | null>(null);
	const shownPosition = $derived(seekDrag ?? playback.position);

	function onSeekInput(e: Event) {
		seekDrag = Number((e.target as HTMLInputElement).value);
	}
	function onSeekCommit(e: Event) {
		const v = Number((e.target as HTMLInputElement).value);
		playback.position = v;
		seekDrag = null;
		api.seek(v);
	}

	const btn =
		'flex size-7 shrink-0 cursor-pointer items-center justify-center rounded-md text-white/80 transition hover:bg-white/15 hover:text-white';
	// The lyrics' shadow. On the container, not text-shadow: the karaoke sweep paints its words with
	// a transparent fill (LyricsView), which a text-shadow would show straight through.
	const lyricsShadow = 'filter:drop-shadow(0 1px 3px rgb(0 0 0/0.9)) drop-shadow(0 0 8px rgb(0 0 0/0.5))';
</script>

<!-- On the window, not the sliders: a range drag that ends with the pointer somewhere else never
     delivers pointerup to the input, and the layer would stay up for good. -->
<svelte:window onpointerup={endDrags} onpointercancel={endDrags} />

<!-- h-screen/w-screen: the window has no chrome, so this fills it exactly and rounds its corners.
     Black under everything: the letterbox of a video that is not 16:9. -->
<!-- svelte-ignore a11y_no_static_element_interactions -- the handlers only track the pointer -->
<div
	class="relative flex h-screen w-screen select-none overflow-hidden rounded-2xl bg-black text-white"
	onpointermove={onMove}
	onpointerleave={() => (inside = false)}
	onfocusin={onFocusIn}
	onfocusout={onFocusOut}
>
	<!-- Owns the music video's element; renders only its zero-sized parking spot. -->
	{#if miniVideo}<VideoSurface fit="fill" />{/if}

	<!-- The picture. With side lyrics it gives up the right ~40% and keeps the rest; the window
	     itself never changes size. -->
	<div class="relative h-full min-w-0 flex-1 overflow-hidden">
		{#if miniVideo}
			<div class="pointer-events-none absolute inset-0" {@attach holdVideo}></div>
		{/if}
		{#if videoUp}
			<!-- The music video above is the picture. -->
		{:else if now?.thumbnail}
			<!-- The cover whole, at full height, over a blurred and darkened copy of itself. Nothing
			     animates over the blur (WebKitGTK re-blurs on repaint), so no cross-fade here: a track
			     change just swaps the two sources. The 64px copy is the one the queue already
			     loaded, and blurred it shows no detail to miss. -->
			<img
				src={thumb(now.thumbnail, 64)}
				alt=""
				class="pointer-events-none absolute inset-0 h-full w-full scale-110 object-cover"
				style="filter:blur(18px) brightness(0.45)"
			/>
			<img
				src={thumb(now.thumbnail, 480)}
				alt=""
				class="pointer-events-none absolute inset-0 h-full w-full object-contain"
			/>
		{:else}
			<div
				class="pointer-events-none absolute inset-0 flex items-center justify-center bg-muted text-muted-foreground/50"
			>
				<HugeiconsIcon icon={MusicNote01Icon} class="h-10 w-10" />
			</div>
		{/if}

		<!-- Overlaid lyrics: the lower half, never in the pointer's way (dragging and the double
		     click go straight through), and they stay when the controls go. `dark` gives them light
		     text whatever the app theme, over a picture that is dark or shaded anyway. -->
		{#if lyricsOpen && !side}
			<div
				class="dark pointer-events-none absolute inset-x-0 bottom-0 flex h-1/2 flex-col px-3 pb-10 text-foreground"
				style="{lyricsShadow};mask-image:linear-gradient(to bottom,transparent 0,#000 25%,#000 85%,transparent 100%);-webkit-mask-image:linear-gradient(to bottom,transparent 0,#000 25%,#000 85%,transparent 100%)"
			>
				<LyricsView compact />
			</div>
		{/if}

		<!-- Position, as a line along the bottom edge while the layer is down (the seek bar takes
		     over while it is up). scaleX rather than width, so a tick repaints without layout. -->
		{#if config.progress}
			<div
				class="pointer-events-none absolute inset-x-0 bottom-0 h-0.5 origin-left bg-primary transition-opacity duration-200 {shown
					? 'opacity-0'
					: ''}"
				style="transform:scaleX({progress})"
			></div>
		{/if}

		<!-- Shade for the controls, along the top and bottom edges only. -->
		<div
			class="pointer-events-none absolute inset-0 transition-opacity duration-200 {shown
				? 'opacity-100'
				: 'opacity-0'}"
			style="background:linear-gradient(to bottom,rgb(0 0 0/0.7) 0,rgb(0 0 0/0) 34%,rgb(0 0 0/0) 66%,rgb(0 0 0/0.7) 100%)"
		></div>

		<!-- The drag handle, and the controls on it. Inset so the window's own resize edges keep the
		     strip along each border (on Linux the GTK border hands its mousedown to the page too);
		     Tauri's drag script stops at buttons and inputs on its own. A double click off the bars
		     goes back to the app, and never maximizes. -->
		<!-- svelte-ignore a11y_no_static_element_interactions -- a mouse-only shortcut; the back button is the accessible way -->
		<div
			data-tauri-drag-region="deep"
			class="absolute inset-y-1.5 left-1.5 flex flex-col justify-between {side ? 'right-0' : 'right-1.5'}"
			onmousedown={onDragDown}
			ondblclick={onDragDblClick}
		>
			<div
				data-mini-bar
				class="flex items-center gap-0.5 p-1 transition-opacity duration-200 {shown
					? 'opacity-100'
					: 'pointer-events-none opacity-0'}"
			>
				<button class={btn} onclick={backToApp} title={t('common.back')} aria-label={t('common.back')}>
					<HugeiconsIcon icon={MaximizeScreenIcon} class="h-4 w-4" />
				</button>
				<!-- Hidden, the lines rest: a hover caught as the layer went down would otherwise keep
				     them looping out of sight. -->
				<div class="mx-1 min-w-0 flex-1 [text-shadow:0_1px_4px_rgb(0_0_0/0.7)]">
					<Marquee
						text={now?.title ?? t('player.not_playing')}
						mode={shown ? appearance.marqueeMini : 'off'}
						class="font-heading text-sm font-semibold leading-tight text-white"
					/>
					<Marquee
						text={now?.artists ?? ''}
						mode={shown ? appearance.marqueeMini : 'off'}
						class="text-xs leading-snug text-white/75"
					/>
				</div>
				<!-- Volume, revealed by hovering it: the slider sits in flow to the left of its icon and
				     grows from zero width, as in MiniPlayer. -->
				<div
					class="flex items-center"
					role="group"
					aria-label={t('a11y.volume')}
					onpointerenter={() => (volHover = true)}
					onpointerleave={() => (volHover = false)}
				>
					<input
						type="range"
						class="range on-art min-w-0 transition-[width,opacity] duration-150 {volOpen
							? 'w-20 opacity-100'
							: 'w-0 opacity-0'}"
						style="--pct:{playback.volume}%"
						min="0"
						max="100"
						value={playback.volume}
						onpointerdown={() => (volDragging = true)}
						oninput={(e) => dragVolume(Number(e.currentTarget.value))}
						onchange={(e) => commitVolume(Number(e.currentTarget.value))}
						onwheel={wheelVolume}
						aria-label={t('a11y.volume')}
					/>
					<button
						class={btn}
						onclick={toggleMute}
						aria-label={playback.volume === 0 ? t('player.unmute') : t('player.mute')}
					>
						<!-- icon swap via altIcon/showAlt — `icon` is frozen at mount -->
						<HugeiconsIcon
							icon={VolumeHighIcon}
							altIcon={VolumeMute02Icon}
							showAlt={playback.volume === 0}
							class="h-4 w-4"
						/>
					</button>
				</div>
				{#if likeable}
					<button
						class={btn}
						onclick={toggleLike}
						aria-label={playback.rating === 'like' ? t('player.remove_from_liked') : t('player.save_to_liked')}
					>
						<span
							class="flex"
							class:animate-heart-pop={justLiked}
							onanimationend={() => (justLiked = false)}
						>
							<HugeiconsIcon
								icon={ThumbsUpIcon}
								class="h-4 w-4 {playback.rating === 'like' ? 'fill-current text-primary' : ''}"
							/>
						</span>
					</button>
					<button
						class={btn}
						onclick={() => toggleNowPlayingRating('dislike')}
						aria-label={playback.rating === 'dislike' ? t('player.remove_dislike') : t('common.dislike')}
					>
						<HugeiconsIcon
							icon={ThumbsDownIcon}
							class="h-4 w-4 {playback.rating === 'dislike' ? 'fill-current' : ''}"
						/>
					</button>
				{/if}
				<!-- Fails when the live widget is not the dynamic one, which this is: nothing to do. -->
				<button
					class={btn}
					onclick={() => api.resetMiniSize().catch(() => {})}
					title={t('player.mini_reset_size')}
					aria-label={t('player.mini_reset_size')}
				>
					<HugeiconsIcon icon={ArrowShrink02Icon} class="h-4 w-4" />
				</button>
			</div>

			<div
				data-mini-bar
				class="flex items-center gap-1 p-1 transition-opacity duration-200 {shown
					? 'opacity-100'
					: 'pointer-events-none opacity-0'}"
			>
				<button class={btn} onclick={() => api.prevTrack()} aria-label={t('a11y.previous')}>
					<HugeiconsIcon icon={PreviousIcon} class="h-4 w-4" />
				</button>
				<button
					class="flex size-8 shrink-0 cursor-pointer items-center justify-center rounded-full bg-white text-black transition-colors hover:bg-white/80"
					onclick={() => api.togglePause()}
					aria-label={playback.paused ? t('player.play') : t('player.pause')}
				>
					<!-- HugeiconsIcon freezes `icon` at mount, so the swap goes through altIcon/showAlt. -->
					<HugeiconsIcon icon={PauseIcon} altIcon={PlayIcon} showAlt={playback.paused} class="h-4 w-4" />
				</button>
				<button class={btn} onclick={() => api.nextTrack()} aria-label={t('a11y.next')}>
					<HugeiconsIcon icon={NextIcon} class="h-4 w-4" />
				</button>
				<input
					type="range"
					class="range on-art mx-1 min-w-0 flex-1"
					style="--pct:{playback.duration ? (shownPosition / playback.duration) * 100 : 0}%"
					min="0"
					max={playback.duration || 0}
					value={shownPosition}
					onpointerdown={() => (seekDragging = true)}
					oninput={onSeekInput}
					onchange={onSeekCommit}
					aria-label={t('a11y.seek')}
				/>
				<button
					class="{btn} {lyricsOpen ? 'text-primary hover:text-primary' : ''}"
					onclick={() => (lyricsOpen = !lyricsOpen)}
					aria-label={t('player.lyrics')}
					aria-pressed={lyricsOpen}
				>
					<HugeiconsIcon icon={Mic01Icon} class="h-4 w-4" />
				</button>
			</div>
		</div>
	</div>

	<!-- Side lyrics: a column of its own on the right, and interactive (a line seeks, the list
	     scrolls). Not a drag handle: the lines are buttons, and the gaps between them would drag. -->
	{#if lyricsOpen && side}
		<div
			class="dark relative flex w-[40%] shrink-0 flex-col bg-black/85 py-2 pr-1 text-foreground"
			style="mask-image:linear-gradient(to bottom,transparent 0,#000 15%,#000 85%,transparent 100%);-webkit-mask-image:linear-gradient(to bottom,transparent 0,#000 15%,#000 85%,transparent 100%)"
		>
			<LyricsView compact />
		</div>
	{/if}
</div>
