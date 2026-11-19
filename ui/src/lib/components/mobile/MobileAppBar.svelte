<script lang="ts">
	// The phone's top bar (Tousync's app-bar pattern): title, optional back chevron, optional
	// trailing slot. Pages configure it through `mobile.svelte.ts` (`configureAppBarUI`) and the
	// config is cleared on navigation by the root layout's afterNavigate.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ArrowLeft01Icon } from '@hugeicons/core-free-icons';
	import { mobile } from '$lib/mobile.svelte';

	function goBack() {
		if (mobile.onAppBarBack) {
			mobile.onAppBarBack();
		} else {
			history.back();
		}
	}
</script>

<header
	class="fixed inset-x-0 top-0 z-30 flex h-14 items-center gap-2 bg-background/95 px-4 backdrop-blur-md"
	style="padding-top: calc(env(safe-area-inset-top, 0px))"
>
	{#if mobile.appBarBack}
		<button
			class="-ml-2 flex h-9 w-9 items-center justify-center rounded-full text-foreground transition hover:bg-accent/20"
			onclick={goBack}
			aria-label="Back"
		>
			<HugeiconsIcon icon={ArrowLeft01Icon} class="h-5 w-5" />
		</button>
	{/if}
	<h2 class="min-w-0 flex-1 truncate text-lg font-semibold">{mobile.appBarTitle}</h2>
	{#each mobile.appBarActions as action (action.label)}
		<button
			class="-mr-1 flex h-9 w-9 items-center justify-center rounded-full text-foreground transition hover:bg-accent/20"
			title={action.label}
			aria-label={action.label}
			onclick={action.onclick}
		>
			<HugeiconsIcon icon={action.icon} class="h-5 w-5" />
		</button>
	{/each}
</header>
