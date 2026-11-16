// Embedded WARP tunnel reactive state (Settings ▸ Network). The backend pushes every change
// through the `warp-status` event; the command hydrates the first paint.
import { listen } from '@tauri-apps/api/event';
import * as api from './api';
import { toast } from './player.svelte';

export const warp = $state({ status: { status: 'off' } as api.WarpStatus });

let inited = false;
export function initWarp() {
	if (inited) return;
	inited = true;
	void api.warpStatus().then((s) => (warp.status = s));
	void listen<api.WarpStatus>('warp-status', (e) => (warp.status = e.payload));
}

/** Whether the switch should read as on: up, or on its way up. */
export const warpOn = $derived(
	warp.status.status === 'connected' ||
		warp.status.status === 'connecting' ||
		warp.status.status === 'registering'
);

/** Optimistic toggle; the `warp-status` events keep the truth flowing in from the backend. */
export async function setWarpEnabled(on: boolean) {
	warp.status = on ? { status: 'registering' } : { status: 'off' };
	try {
		await api.setSetting('warp_enabled', String(on));
	} catch (e) {
		// A failed start leaves the setting on in the backend (a restart retries) — read back
		// whatever it actually decided, and say why.
		const fallback: api.WarpStatus = { status: 'failed', msg: String(e) };
		warp.status = await api.warpStatus().catch(() => fallback);
		toast.error(String(e));
	}
}
