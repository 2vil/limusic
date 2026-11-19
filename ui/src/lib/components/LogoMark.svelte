<script module lang="ts">
	// Kept as a public path for the animated loader.
	export const NOTE = 'M24 75C43 75 53 49 69 49C84 49 87 126 100 152C113 126 116 49 131 49C147 49 157 75 176 75';
</script>

<script lang="ts">
	// One-colour version for cover placeholders: inline so it follows the current theme token.
	let { class: className = '' }: { class?: string } = $props();
</script>

<svg viewBox="0 0 200 200" fill="none" aria-hidden="true" class={className}>
	<path d="M24 75C43 75 53 49 69 49C84 49 87 126 100 152C113 126 116 49 131 49C147 49 157 75 176 75" stroke="currentColor" stroke-width="15" stroke-linecap="round" />
	<path d="M24 104C43 104 52 84 64 84C78 84 86 145 100 170C114 145 122 84 136 84C148 84 157 104 176 104" stroke="currentColor" stroke-width="9" stroke-linecap="round" />
	<path d="M38 36C58 36 72 50 84 74L100 106L116 74C128 50 142 36 162 36" stroke="currentColor" stroke-width="11" stroke-linecap="round" stroke-linejoin="round" />
</svg>
