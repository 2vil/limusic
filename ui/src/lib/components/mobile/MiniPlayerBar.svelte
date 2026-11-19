<script lang="ts">
	// The persistent mini player above the phone's bottom nav (Tousync's dock slot): cover, title
	// and transport. Tapping anywhere but the buttons opens the full NowPlaying view.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { PauseIcon, PlayIcon, SkipForwardIcon } from '@hugeicons/core-free-icons';
	import { playback, np } from '$lib/player.svelte';
	import { thumb } from '$lib/thumb';
	import { t } from '$lib/i18n.svelte';
	import * as api from '$lib/api';
</script>

{#if playback.now}
	<!-- The row sits above the bottom nav; the nav's own safe-area padding stays under it. -->
	<div class="relative z-20 mx-2 mb-1">
		<button
			class="absolute inset-0 z-10 flex w-full items-center rounded-xl"
			aria-label={t('player.open_now_playing')}
			onclick={() => (np.open = true)}
		></button>
		<div
			class="flex items-center gap-3 rounded-xl border border-border/60 bg-card/95 px-3 py-2 shadow-lg backdrop-blur-md"
		>
			<img
				src={thumb(playback.now.thumbnail, 120)}
				alt=""
				class="h-10 w-10 rounded-md bg-muted object-cover"
			/>
			<div class="min-w-0 flex-1">
				<p class="truncate text-sm font-medium">{playback.now.title}</p>
				<p class="truncate text-xs text-muted-foreground">{playback.now.artists}</p>
			</div>
			<div class="z-20 flex items-center">
				<button
					class="flex h-10 w-10 items-center justify-center rounded-full text-foreground"
					aria-label={playback.paused ? t('player.play') : t('player.pause')}
					onclick={() => api.togglePause()}
				>
					<HugeiconsIcon icon={playback.paused ? PlayIcon : PauseIcon} class="h-5 w-5" />
				</button>
				<button
					class="flex h-10 w-10 items-center justify-center rounded-full text-foreground"
					aria-label={t('player.next')}
					onclick={() => api.nextTrack()}
				>
					<HugeiconsIcon icon={SkipForwardIcon} class="h-5 w-5" />
				</button>
			</div>
		</div>
	</div>
{/if}
