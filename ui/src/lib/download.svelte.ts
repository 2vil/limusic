// Download manager reactive state and actions
import * as api from './api';
import type { DownloadTask, DownloadedTrack, SongItem } from './api';
import { toast } from './player.svelte';
import { t } from './i18n.svelte';

export type DownloadState = 'idle' | 'downloading' | 'downloaded';

export const downloadQueue = $state({
	tasks: [] as DownloadTask[],
	isOpen: false,
	get activeCount() {
		return this.tasks.filter(
			(task) =>
				task.status === 'downloading' || task.status === 'queued' || task.status === 'tagging'
		).length;
	}
});

let downloadedSet = $state(new Set<string>());
let downloadedCovers = $state(new Map<string, string>());

export function isDownloaded(videoId: string | undefined | null): boolean {
	if (!videoId) return false;
	return downloadedSet.has(videoId);
}

export function isDownloading(videoId: string | undefined | null): boolean {
	if (!videoId) return false;
	return downloadQueue.tasks.some(
		(task) =>
			task.video_id === videoId &&
			(task.status === 'downloading' || task.status === 'queued' || task.status === 'tagging')
	);
}

export function getSongDownloadState(videoId: string | undefined | null): DownloadState {
	if (!videoId) return 'idle';
	if (isDownloading(videoId)) return 'downloading';
	if (isDownloaded(videoId)) return 'downloaded';
	return 'idle';
}

/** Returns the local cover path for a downloaded track, if available. */
export function getDownloadedCover(videoId: string | undefined | null): string | undefined {
	if (!videoId) return undefined;
	const path = downloadedCovers.get(videoId);
	// Only return local paths (absolute paths starting with /)
	if (path && (path.startsWith('/') || /^[A-Za-z]:[\\/]/.test(path))) return path;
	return undefined;
}

export function getCollectionDownloadState(videoIds: (string | undefined | null)[]): DownloadState {
	if (!videoIds || videoIds.length === 0) return 'idle';
	const validIds = videoIds.filter(Boolean) as string[];
	if (validIds.length === 0) return 'idle';
	if (validIds.some((id) => isDownloading(id))) return 'downloading';
	if (validIds.every((id) => isDownloaded(id))) return 'downloaded';
	return 'idle';
}

let initialized = false;
let scanInFlight = false;

/** Re-read the downloaded ids/tracks from the backend into the reactive set. Called at startup
 * and again when the launch scan (after a reinstall) re-registers files from disk. */
async function refreshDownloadedState() {
	const ids = await api.getDownloadedIds();
	const tracks = await api.getDownloadedTracks();
	const nextSet = new Set<string>(ids);
	const nextCovers = new Map<string, string>();
	for (const track of tracks) {
		nextSet.add(track.video_id);
		if (track.thumbnail) nextCovers.set(track.video_id, track.thumbnail);
	}
	downloadedSet = nextSet;
	downloadedCovers = nextCovers;
}

/** Delete a downloaded track: file off the disk, row off the database, check off the row. */
export async function deleteDownloaded(videoId: string, title: string): Promise<void> {
	try {
		await api.deleteDownloadedFile(videoId);
		const nextSet = new Set(downloadedSet);
		nextSet.delete(videoId);
		downloadedSet = nextSet;
		const nextCovers = new Map(downloadedCovers);
		nextCovers.delete(videoId);
		downloadedCovers = nextCovers;
		downloadQueue.tasks = downloadQueue.tasks.filter((t) => t.video_id !== videoId);
		toast.success(t('download.deleted', { title }));
	} catch (e) {
		toast.error(String(e));
	}
}

/** The Downloads panel's re-scan button: the launch scan, on demand. The backend emits
 * `downloaded-updated` either way, which refreshes the reactive set; returns the count. */
export async function retryTask(taskId: string): Promise<void> {
	try {
		await api.retryDownload(taskId);
	} catch (e) {
		toast.error(String(e));
	}
}

export async function retryAllFailed(): Promise<void> {
	try {
		const queued = await api.retryAllFailedDownloads();
		if (queued > 0) toast.success(t('download.retry_all_queued', { count: queued }));
		else toast.info(t('download.retry_all_none'));
	} catch (e) {
		toast.error(String(e));
	}
}

export async function dismissTask(taskId: string): Promise<void> {
	try {
		await api.dismissFailedDownload(taskId);
		downloadQueue.tasks = downloadQueue.tasks.filter((t) => t.id !== taskId);
	} catch (e) {
		toast.error(String(e));
	}
}

export async function rescanDownloads(): Promise<number> {
	if (scanInFlight) return 0;
	scanInFlight = true;
	try {
		return await api.rescanDownloads();
	} finally {
		scanInFlight = false;
	}
}

export async function initDownloads() {
	if (initialized) return;
	initialized = true;

	// The launch scan (Rust) repopulates the database from disk after a reinstall; when it lands,
	// re-read the ids so already-downloaded tracks stop looking downloadable.
	await api.onDownloadedUpdated(() => void refreshDownloadedState());

	await refreshDownloadedState().catch((e) =>
		console.error('Failed to get initial downloads', e)
	);

	try {
		downloadQueue.tasks = (await api.getDownloads()).filter((t) => t.status !== 'cancelled');
	} catch (e) {
		console.error('Failed to get the download queue', e);
	}

	await api.onDownloadProgress((task) => {
		if (task.status === 'cancelled') {
			downloadQueue.tasks = downloadQueue.tasks.filter((t) => t.id !== task.id);
			return;
		}

		// A retry re-enqueues under a fresh id: drop the failed card it replaces, so the panel
		// never shows both the dead entry and its live successor.
		if (task.status === 'queued' || task.status === 'downloading') {
			downloadQueue.tasks = downloadQueue.tasks.filter(
				(t) => !(t.video_id === task.video_id && t.status === 'failed' && t.id !== task.id)
			);
		}

		const idx = downloadQueue.tasks.findIndex((t) => t.id === task.id);
		if (idx >= 0) {
			const prev = downloadQueue.tasks[idx];
			downloadQueue.tasks[idx] = task;
			if (prev.status !== 'completed' && task.status === 'completed') {
				downloadedSet.add(task.video_id);
				downloadedSet = new Set(downloadedSet);
				if (task.thumbnail) {
					downloadedCovers.set(task.video_id, task.thumbnail);
					downloadedCovers = new Map(downloadedCovers);
				}
				toast(`${t('download.downloaded')}: ${task.title}`);
			} else if (prev.status !== 'failed' && task.status === 'failed') {
				toast(`${t('download.status_failed')}: ${task.title}`);
			}
		} else {
			downloadQueue.tasks.push(task);
			if (task.status === 'completed') {
				downloadedSet.add(task.video_id);
				downloadedSet = new Set(downloadedSet);
				if (task.thumbnail) {
					downloadedCovers.set(task.video_id, task.thumbnail);
					downloadedCovers = new Map(downloadedCovers);
				}
			}
		}
	});
}

export async function downloadSong(song: SongItem, subfolder?: string) {
	try {
		await api.downloadTrack(song, subfolder);
		toast(`${t('download.downloading')}: ${song.title}`);
	} catch (e) {
		toast(`${t('download.status_failed')}: ${e}`);
	}
}

export async function downloadPlaylistById(playlistId: string, title?: string) {
	try {
		toast(t('download.downloading'));
		const count = await api.downloadPlaylist(playlistId, title);
		toast(`${count} ${t('common.songs')} ${t('download.queued')}`);
	} catch (e) {
		toast(`${t('download.status_failed')}: ${e}`);
	}
}

export async function downloadAlbumById(albumId: string, title?: string) {
	try {
		toast(t('download.downloading'));
		const count = await api.downloadAlbum(albumId, title);
		toast(`${count} ${t('common.songs')} ${t('download.queued')}`);
	} catch (e) {
		toast(`${t('download.status_failed')}: ${e}`);
	}
}

export async function cancelTask(id: string) {
	try {
		downloadQueue.tasks = downloadQueue.tasks.filter((t) => t.id !== id);
		await api.cancelDownload(id);
	} catch (e) {
		console.error(e);
	}
}

export async function cancelAll() {
	try {
		downloadQueue.tasks = downloadQueue.tasks.filter(
			(t) =>
				t.status !== 'downloading' && t.status !== 'queued' && t.status !== 'tagging'
		);
		await api.cancelAllDownloads();
	} catch (e) {
		console.error(e);
	}
}

export async function clearCompleted() {
	try {
		await api.clearCompletedDownloads();
		downloadQueue.tasks = downloadQueue.tasks.filter(
			(task) =>
				task.status !== 'completed' && task.status !== 'failed' && task.status !== 'cancelled'
		);
	} catch (e) {
		console.error(e);
	}
}

export async function deleteAllDownloads() {
	try {
		await api.deleteAllDownloads();
		downloadQueue.tasks = [];
		downloadedSet = new Set();
		downloadedCovers = new Map();
	} catch (e) {
		console.error(e);
	}
}
