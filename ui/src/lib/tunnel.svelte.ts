// Embedded WARP tunnel reactive state (Settings ▸ Network). The backend pushes every change
// through the `tunnel-status` event; the command hydrates the first paint.
import { listen } from '@tauri-apps/api/event';
import * as api from './api';
import { toast } from './player.svelte';

export const tunnel = $state({
	status: { status: 'off' } as api.TunnelStatus,
	/** The imported `.conf`'s file name, for display only — the config itself stays in Rust. */
	confName: null as string | null
});

let inited = false;
export function initTunnel() {
	if (inited) return;
	inited = true;
	void api.tunnelStatus().then((s) => (tunnel.status = s));
	void listen<api.TunnelStatus>('tunnel-status', (e) => (tunnel.status = e.payload));
}

/** Whether the switch should read as on: up, or on its way up. A function, not exported
 * `$derived`: Svelte 5 forbids exporting derived state from a module, and reading `$state`
 * inside the call is reactive in templates the same way (the `isDownloaded` pattern). */
export function tunnelOn(): boolean {
	return (
		tunnel.status.status === 'connected' ||
		tunnel.status.status === 'connecting' ||
		tunnel.status.status === 'registering'
	);
}

/** Optimistic toggle; the `tunnel-status` events keep the truth flowing in from the backend. */
export async function setTunnelEnabled(on: boolean) {
	tunnel.status = on ? { status: 'connecting' } : { status: 'off' };
	try {
		await api.setSetting('warp_enabled', String(on));
	} catch (e) {
		// A failed start leaves the setting on in the backend (a restart retries) — read back
		// whatever it actually decided, and say why.
		const fallback: api.TunnelStatus = { status: 'failed', msg: String(e) };
		tunnel.status = await api.tunnelStatus().catch(() => fallback);
		toast.error(String(e));
	}
}

/** Pick a `.conf`, validate and store it (Rust side), and live-swap the tunnel if it is up. */
export async function importTunnelConfig(): Promise<string | null> {
	try {
		const name = await api.tunnelImportConfig();
		tunnel.confName = name;
		return name;
	} catch (e) {
		if (String(e).includes('cancelled')) return null;
		toast.error(String(e));
		return null;
	}
}
