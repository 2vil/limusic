<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		Home01Icon,
		Search01Icon,
		LibraryIcon,
		Download04Icon,
		Settings01Icon
	} from '@hugeicons/core-free-icons';
	import { downloadQueue } from '$lib/download.svelte';
	import { ui } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';

	const isActive = (href: string) =>
		href === '/' ? page.url.pathname === '/' : page.url.pathname.startsWith(href);
</script>

<nav
	data-bottom-nav
	class="flex h-14 shrink-0 items-center justify-around border-t border-border/60 bg-card/95 px-2 pb-[env(safe-area-inset-bottom,0px)] text-xs backdrop-blur-md md:hidden"
	aria-label={t('nav.home')}
>
	<!-- Home -->
	<a
		href="/"
		class="flex flex-1 flex-col items-center justify-center gap-1 py-1 transition-colors {isActive('/')
			? 'font-medium text-primary'
			: 'text-muted-foreground hover:text-foreground'}"
	>
		<HugeiconsIcon icon={Home01Icon} class="h-5 w-5" />
		<span class="text-[11px] leading-none">{t('nav.home')}</span>
	</a>

	<!-- Search -->
	<a
		href="/search"
		onclick={async (e) => {
			if (!isActive('/search') || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
			e.preventDefault();
			await goto(page.url, { keepFocus: true, replaceState: true });
			const input = document.querySelector<HTMLInputElement>('#search-form input');
			input?.focus();
			input?.select();
		}}
		class="flex flex-1 flex-col items-center justify-center gap-1 py-1 transition-colors {isActive('/search')
			? 'font-medium text-primary'
			: 'text-muted-foreground hover:text-foreground'}"
	>
		<HugeiconsIcon icon={Search01Icon} class="h-5 w-5" />
		<span class="text-[11px] leading-none">{t('nav.search')}</span>
	</a>

	<!-- Library -->
	<a
		href="/library"
		class="flex flex-1 flex-col items-center justify-center gap-1 py-1 transition-colors {isActive('/library')
			? 'font-medium text-primary'
			: 'text-muted-foreground hover:text-foreground'}"
	>
		<HugeiconsIcon icon={LibraryIcon} class="h-5 w-5" />
		<span class="text-[11px] leading-none">{t('nav.library')}</span>
	</a>

	<!-- Downloads -->
	<button
		type="button"
		onclick={() => (downloadQueue.isOpen = !downloadQueue.isOpen)}
		class="relative flex flex-1 flex-col items-center justify-center gap-1 py-1 transition-colors {downloadQueue.isOpen || downloadQueue.activeCount > 0
			? 'font-medium text-primary'
			: 'text-muted-foreground hover:text-foreground'}"
	>
		<span class="relative">
			<HugeiconsIcon icon={Download04Icon} class="h-5 w-5" />
			{#if downloadQueue.activeCount > 0}
				<span
					class="absolute -right-2 -top-1 flex h-3.5 w-3.5 items-center justify-center rounded-full bg-primary text-[9px] font-bold text-primary-foreground shadow-sm"
				>
					{downloadQueue.activeCount}
				</span>
			{/if}
		</span>
		<span class="text-[11px] leading-none">{t('download.title')}</span>
	</button>

	<!-- Settings -->
	<button
		type="button"
		onclick={() => (ui.settingsOpen = true)}
		class="flex flex-1 flex-col items-center justify-center gap-1 py-1 transition-colors {ui.settingsOpen
			? 'font-medium text-primary'
			: 'text-muted-foreground hover:text-foreground'}"
	>
		<HugeiconsIcon icon={Settings01Icon} class="h-5 w-5" />
		<span class="text-[11px] leading-none">{t('nav.settings')}</span>
	</button>
</nav>
