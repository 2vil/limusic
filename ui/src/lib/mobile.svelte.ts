// Mobile experience store, modeled on Tousync's src/lib/store.svelte.ts. One reactive object
// drives every phone-specific decision: a JS-driven `isMobile` (<768 px — the layout trees are
// structurally different, so Tailwind classes alone can't do the switch), the platform, and the
// mobile AppBar slots that pages configure imperatively (Tousync's configureAppBarUI pattern).
import { browser } from '$app/environment';
	import type { IconSvgElement } from '@hugeicons/svelte';

/** Breakpoint under which the app renders its phone shell. Matches Tailwind's `md`. */
const MOBILE_BREAKPOINT = 768;

class Mobile {
	#screenWidth = $state(browser ? window.innerWidth : 1024);

	/** True under a phone-width viewport — drives the layout tree switch in +layout.svelte. */
	isMobile = $derived(this.#screenWidth < MOBILE_BREAKPOINT);

	/** Running inside a Tauri webview (desktop window or Android app), not a plain browser. */
	isTauri = $state(browser && '__TAURI_INTERNALS__' in window);

	/** Android app (vs desktop Tauri or browser) — gates desktop-only features. */
	isAndroid = $state(browser && /Android/i.test(navigator.userAgent));

	// --- AppBar slots (mobile top bar) ---------------------------------------------------------
	// Pages configure these imperatively (configureAppBarUI below); the AppBar renders them and
	// `resetAppUI` clears them on navigation so a page never inherits another page's chrome.

	appBarTitle = $state('');
	/** Show the back chevron in the AppBar (routes deeper than the top level). */
	appBarBack = $state(false);
	/** Trailing action buttons (icon objects from @hugeicons/core-free-icons). */
	appBarActions = $state<Array<{ icon: IconSvgElement; label: string; onclick: () => void }>>([]);
	/** When set, the AppBar back chevron (and the Android back button) call this instead of history. */
	onAppBarBack: (() => void) | null = null;
	/** Hide the whole AppBar (fullscreen experiences: NowPlaying covers the top itself). */
	appBarHidden = $state(false);

	constructor() {
		if (!browser) return;
		// Keep isMobile live through resizes and rotations — widening a phone window flips the
		// layout tree the same way a phone flips it on rotation (Tousync does the same).
		window.addEventListener('resize', () => (this.#screenWidth = window.innerWidth));
	}

	configureAppBarUI(options: {
		title?: string;
		back?: boolean;
		onBack?: () => void;
		hidden?: boolean;
		actions?: Array<{ icon: IconSvgElement; label: string; onclick: () => void }>;
	}) {
		if (options.title !== undefined) this.appBarTitle = options.title;
		if (options.back !== undefined) this.appBarBack = options.back;
		this.onAppBarBack = options.onBack ?? null;
		if (options.actions !== undefined) this.appBarActions = options.actions;
		if (options.hidden !== undefined) this.appBarHidden = options.hidden;
	}

	/** Clear per-page chrome on navigation so the next route starts clean. */
	resetAppUI() {
		this.appBarTitle = '';
		this.appBarBack = false;
		this.onAppBarBack = null;
		this.appBarHidden = false;
	}
}

export const mobile = new Mobile();
