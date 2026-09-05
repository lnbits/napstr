<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { open, save } from '@tauri-apps/plugin-dialog';

  let appVersion = '…';
  const SEARCH_PAGE_SIZE = 100;
  const VISIBLE_SEEDER_LIMIT = 100;

  type View = 'Search' | 'Downloads' | 'Shared' | 'Profile' | 'Settings' | 'Trollbox';
  type PlayerMode = 'single' | 'folder' | 'all' | 'shuffle';
  type PlayerOrigin = 'search' | 'downloads' | 'shared' | 'direct';
  type WindowResizeDirection = 'East' | 'North' | 'NorthEast' | 'NorthWest' | 'South' | 'SouthEast' | 'SouthWest' | 'West';
  type Result = {
    id: number;
    name: string;
    format: string;
    size: string;
    bytes: number;
    sources: number;
    speed: string;
    length: string;
    fileId: string;
    sourceDetails?: SourceDetail[];
    remote?: boolean;
    artist?: string;
    album?: string;
    license?: string;
    description?: string;
    tags?: string;
  };
  type SourceDetail = { pubkey: string; npub: string; displayName: string; relay: string; about: string; picture: string; eventId: string };
  type Transfer = {
    id: number;
    fileId: string;
    name: string;
    size: string;
    speed: string;
    progress: number;
    status: string;
    destination: string;
  };

  const views: { label: View; icon: string }[] = [
    { label: 'Search', icon: '⌕' },
    { label: 'Downloads', icon: '⇩' },
    { label: 'Shared', icon: '▤' },
    { label: 'Profile', icon: '☺' },
    { label: 'Settings', icon: '⚙' },
    { label: 'Trollbox', icon: '▣' }
  ];

  type NativeFile = { fileId: string; filename: string; path: string; folder: string; size: number; format: string; status: string; title: string; artist: string; album: string; mime: string; license: string; description: string; tags: string };
  type NativeTransfer = { id: number; fileId: string; filename: string; size: number; progress: number; status: string; speed: string; destination: string };
  type SeedingStat = { fileId: string; delivered: number; activeGrants: number; otherSeeders: number };
  type NativeSettings = { napstrFolder: string; nostrRelays: string; displayName: string; profileAbout: string; profilePicture: string; relaysOverTor: boolean };
  type Snapshot = { files: NativeFile[]; transfers: NativeTransfer[]; settings: NativeSettings; indexedBytes: number; native: boolean };
  type NetworkStatus = { connected: boolean; npub: string; pubkey: string; relayCount: number; relaysViaTor: boolean; torRunning: boolean; torStarting: boolean; torProgress: number; torError: string; error: string };
  type NetworkResult = { fileId: string; filename: string; title: string; artist: string; album: string; format: string; mime: string; size: number; license: string; description: string; tags: string; sources: SourceDetail[] };
  type PlayerTrack = { fileId: string; name: string; folder: string; artist: string; mime: string };
  type PlaybackStatus = { fileId: string; currentTime: number; duration: number; playing: boolean; ended: boolean; error: string };
  type ReleaseStatus = { version: string; url: string };
  type GitHubRelease = { tag_name?: unknown; html_url?: unknown };
  type TrollboxMessage = { eventId: string; pubkey: string; npub: string; displayName: string; content: string; createdAt: number };
  type BlockConfirmation =
    | { kind: 'file'; fileId: string; label: string }
    | { kind: 'user'; pubkey: string; label: string };

  let activeView: View = 'Search';
  let results: Result[] = [];
  let resultPage = 0;
  let query = '';
  let format = 'Audio only';
  let sortKey: 'name' | 'format' | 'bytes' | 'sources' | null = null;
  let sortDirection: 1 | -1 = 1;
  let seedingStats: Record<string, SeedingStat> = {};
  let minimumSources = 1;
  let maximumSize = '';
  let searchedQuery = 'All audio';
  let resultsAreNetwork = false;
  let selected: Result | null = null;
  let advanced = false;
  let paused = false;
  let aboutOpen = false;
  let sourceProfile: SourceDetail | null = null;
  let blockConfirmation: BlockConfirmation | null = null;
  let blockInProgress = false;
  let startingDownloads = new Set<string>();
  let clock = '';
  let desktopRuntime = false;
  let nativeReady = false;
  let activityMessage = 'Starting Napstr…';
  let napstrFolder = '';
  let nostrRelays = 'wss://relay.damus.io, wss://nos.lol, wss://relay.nostr.com, wss://relay.primal.net, wss://relay.snort.social, wss://nostr.mom';
  let relaysOverTor = false;
  let displayName = 'napstr-user';
  let profileAbout = 'Sharing files privately with Napstr. napstr.net';
  let profilePicture = '';
  let backupDialog: 'export' | 'import' | 'import-confirm' | null = null;
  let backupRestoreNpub = '';
  let backupCurrentNpub = '';
  let backupCurrentBackedUp = false;
  let backupAcknowledged = false;
  type ArchivedIdentity = { npub: string; keyringAccount: string; archivedAt: string };
  let archivedIdentities: ArchivedIdentity[] = [];
  let backupPassphrase = '';
  let backupPassphraseRepeat = '';
  let backupImportPath = '';
  let backupBusy = false;
  let backupError = '';
  let indexedBytes = 0;
  let networkConnected = false;
  let torRunning = false;
  let torStarting = false;
  let torProgress = 0;
  let torError = '';
  let identityNpub = '';
  let networkError = '';
  let networkConnectPending = false;
  let newRelease: ReleaseStatus | null = null;
  let trollboxMessages: TrollboxMessage[] = [];
  let trollboxDraft = '';
  let trollboxLoading = false;
  let trollboxSending = false;
  let trollboxError = '';
  let trollboxPollPending = false;
  let trollboxRefreshAgain = false;
  let trollboxLog: HTMLDivElement;
  let trackDiscussionFileId = '';
  let trackDiscussionMessages: TrollboxMessage[] = [];
  let trackDiscussionDraft = '';
  let trackDiscussionLoading = false;
  let trackDiscussionSending = false;
  let trackDiscussionError = '';
  let trackDiscussionPollPending = false;
  let trackDiscussionRefreshAgain = false;
  let trackDiscussionLog: HTMLDivElement;
  let searchAction: 'search' | 'surprise' | 'source' | null = null;
  let rescanPending = false;
  let selectedSource = 0;
  let selectedShared: NativeFile | null = null;
  let selectedTagFile: NativeFile | null = null;
  let tagDraft = '';
  let tagSaving = false;
  let libraryFolderView = '*';
  let playerMode: PlayerMode = 'single';
  let playerOrigin: PlayerOrigin = 'direct';
  let playerQueue: PlayerTrack[] = [];
  let playerQueueIndex = -1;
  let currentTrack: PlayerTrack | null = null;
  let playerPlaying = false;
  let playerLoading = false;
  let playerCurrentTime = 0;
  let playerDuration = 0;
  let playerVolume = 0.85;
  let playerEnded = false;
  let lastPlayerError = '';
  let transferPaneHeight = 119;
  let stopTransferResize = () => {};
  let transfers: Transfer[] = [];

  let sharedFiles: Array<NativeFile & { name: string; readableSize: string; peers: number; delivered: number; otherSeeders: number }> = [];

  const readableSize = (bytes: number) => {
    if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
    if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(bytes >= 100 * 1024 ** 2 ? 0 : 1)} MB`;
    if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`;
    return `${bytes} B`;
  };

  function mapFiles(files: NativeFile[]): Result[] {
    return files.map((file, index) => ({
      id: index + 1, name: file.filename, format: file.format, size: readableSize(file.size), bytes: file.size, sources: 1,
      speed: 'Local', length: '—', fileId: file.fileId, artist: file.artist, album: file.album, license: file.license, description: file.description, tags: file.tags
    }));
  }

  function mapNetworkFiles(files: NetworkResult[]): Result[] {
    const localFileIds = new Set(sharedFiles.map((file) => file.fileId));
    return files.map((file, index) => {
      const local = localFileIds.has(file.fileId);
      return {
        id: index + 1, name: file.title || file.filename, format: file.format, size: readableSize(file.size),
        bytes: file.size, sources: file.sources.length, speed: local ? 'Local' : 'Tor', length: '—', fileId: file.fileId,
        sourceDetails: file.sources, remote: !local, artist: file.artist, album: file.album,
        license: file.license, description: file.description, tags: file.tags
      };
    });
  }

  function matchesType(mime: string, fileFormat: string) {
    return mime.startsWith('audio/') && ['MP3', 'FLAC', 'WAV', 'OGG', 'OPUS'].includes(fileFormat.toUpperCase());
  }

  function matchesSelectedFormat(fileFormat: string) {
    return format === 'Audio only' || fileFormat.toUpperCase() === format;
  }

  function maximumBytes() {
    const match = maximumSize.trim().match(/^(\d+(?:\.\d+)?)\s*(B|KB|MB|GB|TB)?$/i);
    if (!match) return Number.POSITIVE_INFINITY;
    const units: Record<string, number> = { B: 1, KB: 1024, MB: 1024 ** 2, GB: 1024 ** 3, TB: 1024 ** 4 };
    return Number(match[1]) * units[(match[2] || 'B').toUpperCase()];
  }

  function eligibleNetworkMatches(matches: NetworkResult[]) {
    return matches.filter((item) =>
      item.sources.length >= minimumSources
      && item.size <= maximumBytes()
      && matchesType(item.mime, item.format)
      && matchesSelectedFormat(item.format)
    );
  }

  function shuffled<T>(items: T[]) {
    const copy = [...items];
    for (let index = copy.length - 1; index > 0; index -= 1) {
      const random = new Uint32Array(1);
      window.crypto.getRandomValues(random);
      const swapIndex = random[0] % (index + 1);
      [copy[index], copy[swapIndex]] = [copy[swapIndex], copy[index]];
    }
    return copy;
  }

  function isActiveTransfer(transfer: Transfer) {
    return transfer.progress < 100 && !/^(Failed|Cancelled|Refused|All seeders refused)/.test(transfer.status);
  }

  function isCompleteTransfer(transfer: Transfer) {
    return transfer.progress >= 100 && transfer.status === 'Verified · Complete' && Boolean(transfer.destination);
  }

  function isFinishedTransfer(transfer: Transfer) {
    return isCompleteTransfer(transfer) || /^(Failed|Cancelled|Refused|All seeders refused)/.test(transfer.status);
  }

  function mapTransfers(items: NativeTransfer[]): Transfer[] {
    return items.map((transfer) => ({
      id: transfer.id,
      fileId: transfer.fileId,
      name: transfer.filename,
      size: readableSize(transfer.size),
      speed: transfer.speed,
      progress: transfer.progress,
      status: transfer.status,
      destination: transfer.destination
    }));
  }

  function isLocalFile(fileId: string) {
    return sharedFiles.some((file) => file.fileId === fileId);
  }

  function folderName(folder: string) {
    return folder || '(Napstr folder)';
  }

  function libraryFolders() {
    return [...new Set(sharedFiles.map((file) => file.folder))]
      .sort((left, right) => folderName(left).localeCompare(folderName(right)));
  }

  function visibleSharedFiles() {
    return libraryFolderView === '*'
      ? sharedFiles
      : sharedFiles.filter((file) => file.folder === libraryFolderView);
  }

  function resultPageCount() {
    return Math.max(1, Math.ceil(results.length / SEARCH_PAGE_SIZE));
  }

  function toggleSort(key: 'name' | 'format' | 'bytes' | 'sources') {
    if (sortKey !== key) {
      sortKey = key;
      sortDirection = 1;
    } else if (sortDirection === 1) {
      sortDirection = -1;
    } else {
      sortKey = null;
      sortDirection = 1;
    }
    resultPage = 0;
  }

  function sortIndicator(key: 'name' | 'format' | 'bytes' | 'sources') {
    if (sortKey !== key) return '';
    return sortDirection === 1 ? ' ▲' : ' ▼';
  }

  function sortedResults(): Result[] {
    const key = sortKey;
    if (!key) return results;
    const direction = sortDirection;
    return [...results].sort((left, right) => {
      if (key === 'bytes' || key === 'sources') return (left[key] - right[key]) * direction;
      return left[key].localeCompare(right[key]) * direction;
    });
  }

  function paginatedResults() {
    const start = resultPage * SEARCH_PAGE_SIZE;
    return sortedResults().slice(start, start + SEARCH_PAGE_SIZE);
  }

  function resultRange() {
    if (!results.length) return '0';
    const start = resultPage * SEARCH_PAGE_SIZE + 1;
    return `${start}–${Math.min(start + SEARCH_PAGE_SIZE - 1, results.length)}`;
  }

  function changeResultPage(nextPage: number) {
    resultPage = Math.max(0, Math.min(nextPage, resultPageCount() - 1));
    selectResult(paginatedResults()[0] ?? null);
  }

  function toPlayerTrack(file: NativeFile): PlayerTrack {
    return {
      fileId: file.fileId,
      name: file.title || file.filename,
      folder: file.folder,
      artist: file.artist,
      mime: file.mime
    };
  }

  function selectTagFile(file: NativeFile) {
    selectedTagFile = { ...file };
    tagDraft = file.tags;
  }

  async function saveTags() {
    if (!nativeReady || !selectedTagFile || tagSaving) return;
    const fileId = selectedTagFile.fileId;
    tagSaving = true;
    try {
      applySnapshot(await invoke<Snapshot>('save_file_tags', { fileId, tags: tagDraft }));
      selectedTagFile = sharedFiles.find((file) => file.fileId === fileId) ?? null;
      tagDraft = selectedTagFile?.tags ?? '';
      activityMessage = 'Tags saved locally';
      if (networkConnected) {
        try {
          await invoke('publish_catalogue');
          activityMessage = 'Tags saved and published to Nostr';
        } catch (error) {
          activityMessage = `Tags saved locally · Nostr publication will retry later: ${String(error)}`;
        }
      }
    } catch (error) {
      activityMessage = `Could not save tags: ${String(error)}`;
    } finally {
      tagSaving = false;
    }
  }

  function sortedLibraryTracks() {
    return sharedFiles
      .map(toPlayerTrack)
      .sort((left, right) => left.folder.localeCompare(right.folder) || left.name.localeCompare(right.name));
  }

  function contextualPlayerQueue(track: PlayerTrack, origin: PlayerOrigin) {
    let queue: PlayerTrack[] = [];
    if (origin === 'downloads') {
      queue = sharedFiles.map(toPlayerTrack);
    } else if (origin === 'search') {
      queue = results
        .filter((result) => isLocalFile(result.fileId))
        .flatMap((result) => {
          const file = sharedFiles.find((item) => item.fileId === result.fileId);
          return file ? [toPlayerTrack(file)] : [];
        });
    } else if (origin === 'shared') {
      queue = visibleSharedFiles().map(toPlayerTrack);
    }
    return queue.some((item) => item.fileId === track.fileId) ? queue : [track];
  }

  function shuffledQueueFrom(tracks: PlayerTrack[], current: PlayerTrack) {
    return [current, ...shuffled(tracks.filter((item) => item.fileId !== current.fileId))];
  }

  function queueForTrack(track: PlayerTrack, mode: PlayerMode, origin: PlayerOrigin = playerOrigin) {
    const library = sortedLibraryTracks();
    if (!library.some((item) => item.fileId === track.fileId)) return [track];
    const contextualQueue = contextualPlayerQueue(track, origin);
    if (origin !== 'direct') {
      if (mode === 'folder') return contextualQueue.filter((item) => item.folder === track.folder);
      if (mode === 'shuffle') return shuffledQueueFrom(contextualQueue, track);
      return contextualQueue;
    }
    if (mode === 'all') return library;
    if (mode === 'shuffle') return shuffledQueueFrom(library, track);
    if (mode === 'folder') return library.filter((item) => item.folder === track.folder);
    return [track];
  }

  function selectPlayingTrack(track: PlayerTrack) {
    if (playerOrigin === 'search') {
      const index = results.findIndex((item) => item.fileId === track.fileId);
      if (index >= 0) {
        resultPage = Math.floor(index / SEARCH_PAGE_SIZE);
        selectResult(results[index]);
      }
    } else if (playerOrigin === 'downloads') {
      const file = sharedFiles.find((item) => item.fileId === track.fileId);
      if (file) selectTagFile(file);
    } else if (playerOrigin === 'shared') {
      const file = sharedFiles.find((item) => item.fileId === track.fileId);
      if (file) selectedShared = { ...file };
    }
  }

  function formatPlayerTime(seconds: number) {
    if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
    const whole = Math.floor(seconds);
    return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`;
  }

  async function loadPlayerTrack(index: number) {
    const track = playerQueue[index];
    if (!track || playerLoading) return;
    playerLoading = true;
    playerQueueIndex = index;
    currentTrack = track;
    selectPlayingTrack(track);
    playerCurrentTime = 0;
    playerDuration = 0;
    playerEnded = false;
    try {
      applyPlaybackStatus(await invoke<PlaybackStatus>('play_audio', { fileId: track.fileId, volume: playerVolume }));
      if (!lastPlayerError) activityMessage = `Playing ${track.name}${track.folder ? ` · ${track.folder}` : ''}`;
    } catch (error) {
      playerPlaying = false;
      playerEnded = true;
      lastPlayerError = String(error);
      activityMessage = `Playback failed: ${lastPlayerError}`;
    } finally {
      playerLoading = false;
    }
  }

  async function playAudio(fileId: string, name: string, mode: PlayerMode = playerMode, origin: PlayerOrigin = 'direct') {
    if (!nativeReady || !fileId) return;
    const indexed = sharedFiles.find((file) => file.fileId === fileId);
    const track = indexed
      ? toPlayerTrack(indexed)
      : { fileId, name, folder: '', artist: '', mime: '' };
    playerOrigin = origin;
    playerMode = indexed ? mode : 'single';
    playerQueue = queueForTrack(track, playerMode, playerOrigin);
    const index = Math.max(0, playerQueue.findIndex((item) => item.fileId === fileId));
    await loadPlayerTrack(index);
  }

  async function togglePlayer() {
    if (!currentTrack) {
      if (activeView === 'Downloads' && selectedTagFile) await playAudio(selectedTagFile.fileId, selectedTagFile.filename, playerMode, 'downloads');
      else if (activeView === 'Shared' && selectedShared) await playAudio(selectedShared.fileId, selectedShared.filename, playerMode, 'shared');
      else if (activeView === 'Search' && selected && isLocalFile(selected.fileId)) await playAudio(selected.fileId, selected.name, playerMode, 'search');
      else activityMessage = 'Select a local song to play';
      return;
    }
    if (playerEnded) {
      await loadPlayerTrack(playerQueueIndex);
      return;
    }
    try { applyPlaybackStatus(await invoke<PlaybackStatus>('toggle_audio')); }
    catch (error) { activityMessage = `Playback failed: ${String(error)}`; }
  }

  async function stopPlayer() {
    try { applyPlaybackStatus(await invoke<PlaybackStatus>('stop_audio')); }
    catch (error) { activityMessage = `Could not stop playback: ${String(error)}`; return; }
    playerEnded = false;
    if (currentTrack) activityMessage = `Stopped ${currentTrack.name}`;
  }

  async function nextPlayerTrack() {
    if (playerQueueIndex + 1 < playerQueue.length) await loadPlayerTrack(playerQueueIndex + 1);
    else stopPlayer();
  }

  async function previousPlayerTrack() {
    if (playerCurrentTime > 3 || playerQueueIndex <= 0) {
      try { applyPlaybackStatus(await invoke<PlaybackStatus>('seek_audio', { seconds: 0 })); }
      catch (error) { activityMessage = `Could not rewind playback: ${String(error)}`; }
      return;
    }
    await loadPlayerTrack(playerQueueIndex - 1);
  }

  async function playerTrackEnded() {
    playerPlaying = false;
    playerEnded = true;
    if (playerMode !== 'single' && playerQueueIndex + 1 < playerQueue.length) {
      await loadPlayerTrack(playerQueueIndex + 1);
    }
  }

  function changePlayerMode() {
    window.localStorage.setItem('napstr-player-mode', playerMode);
    if (!currentTrack) return;
    playerQueue = queueForTrack(currentTrack, playerMode, playerOrigin);
    playerQueueIndex = Math.max(0, playerQueue.findIndex((item) => item.fileId === currentTrack?.fileId));
  }

  async function seekPlayer(event: Event) {
    try {
      applyPlaybackStatus(await invoke<PlaybackStatus>('seek_audio', { seconds: Number((event.currentTarget as HTMLInputElement).value) }));
      playerEnded = false;
    } catch (error) { activityMessage = `Could not seek in this track: ${String(error)}`; }
  }

  function changePlayerVolume(event: Event) {
    playerVolume = Number((event.currentTarget as HTMLInputElement).value);
    if (currentTrack) invoke<PlaybackStatus>('set_audio_volume', { volume: playerVolume }).catch(() => {});
    window.localStorage.setItem('napstr-player-volume', String(playerVolume));
  }

  function applyPlaybackStatus(status: PlaybackStatus) {
    if (!currentTrack || status.fileId !== currentTrack.fileId) return;
    if (status.error) {
      playerPlaying = false;
      playerEnded = true;
      if (status.error !== lastPlayerError) activityMessage = `Playback failed: ${status.error}`;
      lastPlayerError = status.error;
      return;
    }
    lastPlayerError = '';
    playerCurrentTime = status.currentTime;
    playerDuration = status.duration;
    playerPlaying = status.playing;
  }

  function syncResultLocality() {
    const selectedFileId = selected?.fileId;
    if (resultsAreNetwork) {
      results = results.map((result) => {
        const local = isLocalFile(result.fileId);
        return { ...result, remote: !local, speed: local ? 'Local' : 'Tor' };
      });
    } else if (!query.trim() || searchedQuery === 'local catalogue' || searchedQuery === 'All audio') {
      results = mapFiles(sharedFiles);
    } else {
      results = results.filter((result) => isLocalFile(result.fileId));
    }
    resultPage = Math.min(resultPage, resultPageCount() - 1);
    selected = (selectedFileId ? results.find((result) => result.fileId === selectedFileId) : null) ?? paginatedResults()[0] ?? null;
  }

  function applySnapshot(snapshot: Snapshot) {
    nativeReady = snapshot.native;
    indexedBytes = snapshot.indexedBytes;
    napstrFolder = snapshot.settings.napstrFolder;
    nostrRelays = snapshot.settings.nostrRelays;
    relaysOverTor = snapshot.settings.relaysOverTor;
    displayName = snapshot.settings.displayName;
    profileAbout = snapshot.settings.profileAbout;
    profilePicture = snapshot.settings.profilePicture;
    sharedFiles = snapshot.files.map((file) => ({ ...file, name: file.filename, readableSize: readableSize(file.size), peers: seedingStats[file.fileId]?.activeGrants ?? 0, delivered: seedingStats[file.fileId]?.delivered ?? 0, otherSeeders: seedingStats[file.fileId]?.otherSeeders ?? 0 }));
    if (selectedShared) selectedShared = snapshot.files.find((file) => file.fileId === selectedShared?.fileId) ?? null;
    results = mapFiles(snapshot.files);
    resultPage = 0;
    resultsAreNetwork = false;
    selected = results[0] ?? null;
    searchedQuery = 'local catalogue';
    transfers = mapTransfers(snapshot.transfers);
    activityMessage = snapshot.files.length ? `${snapshot.files.length} local file(s) indexed and ready` : 'Choose a Napstr folder to begin';
  }

  async function refreshSnapshot() {
    try { applySnapshot(await invoke<Snapshot>('get_snapshot')); } catch { nativeReady = false; }
  }

  function parseSemver(value: string) {
    const match = value.trim().match(/^v?(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/);
    if (!match) return null;
    return { numbers: match.slice(1, 4).map(Number), prerelease: match[4]?.split('.') ?? [] };
  }

  function compareSemver(leftValue: string, rightValue: string) {
    const left = parseSemver(leftValue);
    const right = parseSemver(rightValue);
    if (!left || !right) return 0;
    for (let index = 0; index < 3; index += 1) {
      if (left.numbers[index] !== right.numbers[index]) return left.numbers[index] > right.numbers[index] ? 1 : -1;
    }
    if (!left.prerelease.length || !right.prerelease.length) {
      return left.prerelease.length === right.prerelease.length ? 0 : left.prerelease.length ? -1 : 1;
    }
    const length = Math.max(left.prerelease.length, right.prerelease.length);
    for (let index = 0; index < length; index += 1) {
      const leftPart = left.prerelease[index];
      const rightPart = right.prerelease[index];
      if (leftPart === undefined || rightPart === undefined) return leftPart === undefined ? -1 : 1;
      if (leftPart === rightPart) continue;
      const leftNumber = /^\d+$/.test(leftPart) ? Number(leftPart) : null;
      const rightNumber = /^\d+$/.test(rightPart) ? Number(rightPart) : null;
      if (leftNumber !== null && rightNumber !== null) return leftNumber > rightNumber ? 1 : -1;
      if (leftNumber !== null || rightNumber !== null) return leftNumber !== null ? -1 : 1;
      return leftPart > rightPart ? 1 : -1;
    }
    return 0;
  }

  function validNapstrReleaseUrl(value: unknown): value is string {
    return typeof value === 'string' && /^https:\/\/github\.com\/lnbits\/napstr\/releases\/tag\/[0-9A-Za-z._+-]{1,100}$/.test(value);
  }

  async function checkForNewRelease() {
    const cacheKey = 'napstr-latest-release';
    let cachedRelease: GitHubRelease | null = null;
    let release: GitHubRelease | null = null;
    try {
      const cached = JSON.parse(window.localStorage.getItem(cacheKey) ?? 'null') as { checkedAt?: number; release?: GitHubRelease | null } | null;
      cachedRelease = cached?.release ?? null;
    } catch { /* ignore invalid old cache data */ }
    if (relaysOverTor) {
      // The update check is a direct clearnet request; in privacy mode rely on the cache only.
      release = cachedRelease;
      if (typeof release?.tag_name !== 'string' || !validNapstrReleaseUrl(release.html_url)) return;
      if (compareSemver(release.tag_name, appVersion) > 0) {
        newRelease = { version: release.tag_name.replace(/^v/, ''), url: release.html_url };
      }
      return;
    }
    try {
      const response = await fetch('https://api.github.com/repos/lnbits/napstr/releases/latest', {
        headers: { Accept: 'application/vnd.github+json' }
      });
      if (!response.ok) throw new Error(`GitHub returned ${response.status}`);
      release = await response.json() as GitHubRelease;
      window.localStorage.setItem(cacheKey, JSON.stringify({ checkedAt: Date.now(), release }));
    } catch { release = cachedRelease; }
    if (typeof release?.tag_name !== 'string' || !validNapstrReleaseUrl(release.html_url)) return;
    if (compareSemver(release.tag_name, appVersion) > 0) {
      newRelease = { version: release.tag_name.replace(/^v/, ''), url: release.html_url };
    }
  }

  async function openNewRelease() {
    if (!newRelease) return;
    try {
      await invoke('open_release_url', { url: newRelease.url });
    } catch (error) {
      activityMessage = `Could not open the release page: ${String(error)}`;
    }
  }

  function chatNameColor(npub: string) {
    const colours = ['#0000b8', '#006400', '#8b008b', '#a00020', '#005f73', '#7a3e00', '#4b0082', '#006b3c', '#9b1c00', '#0047ab', '#7030a0', '#007070'];
    let hash = 2166136261;
    for (let index = 0; index < npub.length; index += 1) {
      hash ^= npub.charCodeAt(index);
      hash = Math.imul(hash, 16777619);
    }
    return colours[(hash >>> 0) % colours.length];
  }

  async function refreshTrollbox() {
    if (!nativeReady || !networkConnected) return;
    if (trollboxPollPending) {
      trollboxRefreshAgain = true;
      return;
    }
    trollboxPollPending = true;
    trollboxLoading = trollboxMessages.length === 0;
    const stayAtBottom = !trollboxLog || trollboxLog.scrollHeight - trollboxLog.scrollTop - trollboxLog.clientHeight < 45;
    try {
      const messages = await invoke<TrollboxMessage[]>('get_trollbox_messages');
      const changed = messages.length !== trollboxMessages.length || messages.at(-1)?.eventId !== trollboxMessages.at(-1)?.eventId;
      trollboxMessages = messages;
      trollboxError = '';
      if (changed && stayAtBottom) {
        await tick();
        trollboxLog?.scrollTo({ top: trollboxLog.scrollHeight });
      }
    } catch (error) {
      trollboxError = String(error);
    } finally {
      trollboxLoading = false;
      trollboxPollPending = false;
      if (trollboxRefreshAgain) {
        trollboxRefreshAgain = false;
        void refreshTrollbox();
      }
    }
  }

  function activateView(view: View) {
    activeView = view;
    if (view === 'Trollbox') void refreshTrollbox();
  }

  async function sendTrollboxMessage() {
    const content = trollboxDraft.trim();
    if (!content || trollboxSending || !networkConnected) return;
    trollboxSending = true;
    trollboxError = '';
    try {
      await invoke('send_trollbox_message', { content });
      trollboxDraft = '';
      await refreshTrollbox();
    } catch (error) {
      trollboxError = String(error);
    } finally {
      trollboxSending = false;
    }
  }

  function blockTrollboxUser(message: TrollboxMessage) {
    if (!nativeReady || message.npub === identityNpub) return;
    blockConfirmation = { kind: 'user', pubkey: message.pubkey, label: message.displayName };
  }

  function selectResult(item: Result | null, forceSubscribe = false) {
    const changed = selected?.fileId !== item?.fileId;
    selected = item;
    selectedSource = 0;
    if (!item) {
      trackDiscussionFileId = '';
      trackDiscussionMessages = [];
      trackDiscussionDraft = '';
      trackDiscussionError = '';
    } else if (changed || forceSubscribe) {
      void refreshTrackDiscussion(item.fileId, true);
    }
  }

  async function refreshTrackDiscussion(fileId = selected?.fileId ?? '', subscribe = false) {
    if (!fileId || !nativeReady || !networkConnected) return;
    if (trackDiscussionFileId !== fileId) {
      trackDiscussionFileId = fileId;
      trackDiscussionMessages = [];
      trackDiscussionDraft = '';
      trackDiscussionError = '';
      subscribe = true;
    }
    if (trackDiscussionPollPending && !subscribe) {
      trackDiscussionRefreshAgain = true;
      return;
    }
    trackDiscussionPollPending = true;
    trackDiscussionLoading = trackDiscussionMessages.length === 0;
    const stayAtBottom = !trackDiscussionLog || trackDiscussionLog.scrollHeight - trackDiscussionLog.scrollTop - trackDiscussionLog.clientHeight < 35;
    try {
      const messages = await invoke<TrollboxMessage[]>('get_track_discussion_messages', { fileId, subscribe });
      if (selected?.fileId !== fileId || trackDiscussionFileId !== fileId) return;
      const changed = messages.length !== trackDiscussionMessages.length || messages.at(-1)?.eventId !== trackDiscussionMessages.at(-1)?.eventId;
      trackDiscussionMessages = messages;
      trackDiscussionError = '';
      if (changed && stayAtBottom) {
        await tick();
        trackDiscussionLog?.scrollTo({ top: trackDiscussionLog.scrollHeight });
      }
    } catch (error) {
      if (selected?.fileId === fileId) trackDiscussionError = String(error);
    } finally {
      if (selected?.fileId === fileId) {
        trackDiscussionLoading = false;
        trackDiscussionPollPending = false;
        if (trackDiscussionRefreshAgain) {
          trackDiscussionRefreshAgain = false;
          void refreshTrackDiscussion(fileId);
        }
      }
    }
  }

  async function sendTrackDiscussionMessage() {
    const fileId = selected?.fileId;
    const content = trackDiscussionDraft.trim();
    if (!fileId || !content || trackDiscussionSending || !networkConnected) return;
    trackDiscussionSending = true;
    trackDiscussionError = '';
    try {
      await invoke('send_track_discussion_message', { fileId, content });
      if (selected?.fileId !== fileId) return;
      trackDiscussionDraft = '';
      await refreshTrackDiscussion(fileId);
    } catch (error) {
      if (selected?.fileId === fileId) trackDiscussionError = String(error);
    } finally {
      trackDiscussionSending = false;
    }
  }

  async function refreshLocalLibrary() {
    try {
      const snapshot = await invoke<Snapshot>('get_snapshot');
      indexedBytes = snapshot.indexedBytes;
      const nextFiles = snapshot.files.map((file) => ({ ...file, name: file.filename, readableSize: readableSize(file.size), peers: seedingStats[file.fileId]?.activeGrants ?? 0, delivered: seedingStats[file.fileId]?.delivered ?? 0, otherSeeders: seedingStats[file.fileId]?.otherSeeders ?? 0 }));
      const removedCurrentTrack = currentTrack && !nextFiles.some((file) => file.fileId === currentTrack?.fileId);
      sharedFiles = nextFiles;
      if (selectedShared) selectedShared = nextFiles.find((file) => file.fileId === selectedShared?.fileId) ?? null;
      if (selectedTagFile && !nextFiles.some((file) => file.fileId === selectedTagFile?.fileId)) {
        selectedTagFile = null;
        tagDraft = '';
      }
      if (removedCurrentTrack) {
        invoke<PlaybackStatus>('stop_audio').catch(() => {});
        currentTrack = null;
        playerQueue = [];
        playerQueueIndex = -1;
        playerPlaying = false;
        playerLoading = false;
        playerCurrentTime = 0;
        playerDuration = 0;
        activityMessage = 'Stopped playback because the file was removed from the Napstr folder';
      } else if (currentTrack) {
        playerQueue = queueForTrack(currentTrack, playerMode);
        playerQueueIndex = playerQueue.findIndex((item) => item.fileId === currentTrack?.fileId);
      }
      syncResultLocality();
    } catch { /* the next folder-watch or transfer poll will retry */ }
  }

  async function connectNetwork() {
    if (!nativeReady || networkConnectPending) return;
    networkConnectPending = true;
    activityMessage = 'Connecting to the music network and opening your encrypted inbox…';
    try {
      const status = await invoke<NetworkStatus>('start_network');
      applyNetworkStatus(status);
      activityMessage = status.relaysViaTor
        ? 'Connected privately through Tor · loading the most available music…'
        : 'Connected · loading the most available music…';
      await search();
      if (status.torError) activityMessage = `Tor failed: ${status.torError} · click the connection panel to retry`;
    } catch (error) {
      networkConnected = false;
      networkError = String(error);
      activityMessage = `Network unavailable: ${String(error)}`;
    } finally {
      networkConnectPending = false;
    }
  }

  function applyNetworkStatus(status: NetworkStatus) {
    networkConnected = status.connected;
    torRunning = status.torRunning;
    torStarting = status.torStarting;
    torProgress = status.torProgress;
    torError = status.torError;
    identityNpub = status.npub;
    networkError = status.error;
  }

  async function recoverAfterSleep() {
    if (!nativeReady) return;
    networkConnected = false;
    torRunning = false;
    torStarting = true;
    torProgress = 0;
    playerPlaying = false;
    playerLoading = false;
    playerCurrentTime = 0;
    playerEnded = currentTrack !== null;
    lastPlayerError = '';
    activityMessage = 'Computer resumed · reconnecting Nostr, Tor, and audio…';
    try {
      await invoke('recover_after_sleep');
    } catch (error) {
      activityMessage = `Resume recovery failed: ${String(error)} · click the connection panel to retry`;
    }
  }

  function torStatusLabel() {
    if (torRunning) return 'Tor connected';
    if (torError) return 'Tor failed';
    if (torStarting && torProgress > 0) return `Tor connecting ${torProgress}%`;
    return nativeReady ? 'Tor connecting' : 'Tor unavailable';
  }

  async function search() {
    if (searchAction) return;
    searchAction = 'search';
    try {
      searchedQuery = query.trim() || 'All audio';
      if (networkConnected) {
        try {
          const matches = await invoke<NetworkResult[]>('network_search', { query: query.trim() });
          const ranked = eligibleNetworkMatches(matches)
            .sort((left, right) => right.sources.length - left.sources.length || left.filename.localeCompare(right.filename));
          results = mapNetworkFiles(ranked);
          resultsAreNetwork = true;
          activityMessage = `${results.length} track(s) found across the network, most available first`;
        } catch (error) { activityMessage = `Global search failed: ${String(error)}`; }
      } else if (nativeReady) {
        try {
          const matches = await invoke<NativeFile[]>('search_catalog', { query: query.trim() });
          results = mapFiles(matches.filter((item) => minimumSources <= 1 && item.size <= maximumBytes() && matchesType(item.mime, item.format) && matchesSelectedFormat(item.format)));
          resultsAreNetwork = false;
          activityMessage = `${results.length} local match(es) found`;
        } catch (error) { activityMessage = `Search failed: ${String(error)}`; }
      }
      resultPage = 0;
      selectResult(results[0] ?? null, true);
    } finally {
      searchAction = null;
    }
  }

  async function surpriseMe() {
    if (searchAction) return;
    if (!networkConnected) {
      activityMessage = 'Connect first, then ask for a surprise';
      return;
    }
    searchAction = 'surprise';
    searchedQuery = 'Surprise me';
    activityMessage = 'Finding 50 random downloadable tracks…';
    try {
      const matches = await invoke<NetworkResult[]>('network_search', { query: '' });
      const downloadable = eligibleNetworkMatches(matches)
        .filter((item) => !isLocalFile(item.fileId) && item.sources.length > 0);
      results = mapNetworkFiles(shuffled(downloadable).slice(0, 50));
      resultsAreNetwork = true;
      resultPage = 0;
      selectResult(results[0] ?? null, true);
      activityMessage = results.length
        ? `${results.length} random downloadable track${results.length === 1 ? '' : 's'} found`
        : 'No downloadable tracks are currently available';
    } catch (error) {
      activityMessage = `Surprise search failed: ${String(error)}`;
    } finally {
      searchAction = null;
    }
  }

  async function showSourceCatalogue(profile: SourceDetail) {
    if (searchAction || !networkConnected) return;
    searchAction = 'source';
    const label = profile.displayName || `${profile.npub.slice(0, 12)}…`;
    searchedQuery = `Music shared by ${label}`;
    activityMessage = `Loading everything shared by ${label}…`;
    try {
      const matches = await invoke<NetworkResult[]>('network_search', { query: '' });
      const shared = matches
        .filter((item) => item.sources.some((source) => source.pubkey === profile.pubkey))
        .sort((left, right) => left.filename.localeCompare(right.filename));
      results = mapNetworkFiles(shared);
      resultsAreNetwork = true;
      resultPage = 0;
      selectResult(results[0] ?? null, true);
      sourceProfile = null;
      activeView = 'Search';
      activityMessage = shared.length
        ? `${shared.length} track${shared.length === 1 ? '' : 's'} shared by ${label}`
        : `${label} is not sharing anything right now`;
    } catch (error) {
      activityMessage = `Could not load their shared music: ${String(error)}`;
    } finally {
      searchAction = null;
    }
  }

  async function startDownload() {
    const target = selected;
    if (!target) return;
    if (nativeReady && isLocalFile(target.fileId)) {
      await playAudio(target.fileId, target.name, playerMode, 'search');
      return;
    }
    const activeTransfer = transfers.find((item) => item.fileId === target.fileId && isActiveTransfer(item));
    if (activeTransfer || startingDownloads.has(target.fileId)) {
      activityMessage = `${target.name} is already downloading`;
      return;
    }
    if (nativeReady) {
      const sources = target.sourceDetails ?? [];
      if (!sources.length) { activityMessage = 'No seeder is available for this file'; return; }
      startingDownloads = new Set(startingDownloads).add(target.fileId);
      transfers = [{
        id: Date.now(), fileId: target.fileId, name: target.name, size: target.size,
        speed: 'Contacting sources…', progress: 0, status: 'Sending encrypted request', destination: ''
      }, ...transfers];
      const candidateCount = Math.min(sources.length, 3);
      activityMessage = `Asking ${candidateCount} source${candidateCount === 1 ? '' : 's'} for this track · the fastest private connection wins…`;
      try {
        await invoke('request_network_download', { fileId: target.fileId, sourcePubkeys: sources.map((source) => source.pubkey) });
        transfers = mapTransfers(await invoke<NativeTransfer[]>('get_transfers'));
        activityMessage = 'Seeder race started · the fastest responsive source will stream the file';
      } catch (error) {
        try { transfers = mapTransfers(await invoke<NativeTransfer[]>('get_transfers')); }
        catch { transfers = transfers.filter((item) => item.fileId !== target.fileId); }
        activityMessage = `Request failed: ${String(error)}`;
      } finally {
        const nextStarting = new Set(startingDownloads);
        nextStarting.delete(target.fileId);
        startingDownloads = nextStarting;
      }
      return;
    }
  }

  async function playSelectedAudio() {
    if (!selected || !isLocalFile(selected.fileId)) return;
    await playAudio(selected.fileId, selected.name, playerMode, 'search');
  }

  async function playSelectedSharedAudio() {
    if (!selectedShared) return;
    await playAudio(selectedShared.fileId, selectedShared.filename, playerMode, 'shared');
  }

  async function playSelectedFolder() {
    if (!selectedShared) return;
    await playAudio(selectedShared.fileId, selectedShared.filename, 'folder', 'shared');
  }

  async function playAllSongs() {
    const first = selectedShared ?? visibleSharedFiles()[0] ?? sharedFiles[0];
    if (!first) return;
    await playAudio(first.fileId, first.filename, 'all', 'shared');
  }

  async function activateSelected() {
    if (nativeReady && selected && isLocalFile(selected.fileId)) await playSelectedAudio();
    else await startDownload();
  }

  function blockSelectedFile() {
    if (!nativeReady || !selected?.remote) return;
    blockConfirmation = { kind: 'file', fileId: selected.fileId, label: selected.name };
  }

  function blockSelectedUser() {
    const source = selected?.sourceDetails?.[selectedSource];
    if (!nativeReady || !source) return;
    blockConfirmation = { kind: 'user', pubkey: source.pubkey, label: source.displayName };
  }

  async function confirmBlock() {
    if (!blockConfirmation || blockInProgress) return;
    const target = blockConfirmation;
    blockInProgress = true;
    try {
      if (target.kind === 'file') {
        await invoke('block_file', { fileId: target.fileId });
        activityMessage = 'File hash blocked locally';
      } else {
        await invoke('block_user', { pubkey: target.pubkey });
        activityMessage = 'Nostr publisher blocked locally';
      }
      blockConfirmation = null;
      if (activeView === 'Trollbox') {
        trollboxMessages = trollboxMessages.filter((message) => message.pubkey !== ('pubkey' in target ? target.pubkey : ''));
        await refreshTrollbox();
      } else {
        await search();
      }
    } catch (error) {
      activityMessage = `Could not block ${target.kind}: ${String(error)}`;
    } finally {
      blockInProgress = false;
    }
  }

  async function removeTransfer(id: number) {
    if (nativeReady) {
      try { await invoke('cancel_transfer', { id }); await invoke('remove_transfer', { id }); } catch (error) { activityMessage = `Could not remove transfer: ${String(error)}`; }
    }
    transfers = transfers.filter((transfer) => transfer.id !== id);
    if (nativeReady) await refreshLocalLibrary();
  }

  async function clearFinishedTransfers() {
    const finished = transfers.filter(isFinishedTransfer);
    if (!finished.length) return;
    const removed = new Set<number>();
    for (const transfer of finished) {
      try {
        if (nativeReady) await invoke('remove_transfer', { id: transfer.id });
        removed.add(transfer.id);
      } catch (error) {
        activityMessage = `Could not clear every finished transfer: ${String(error)}`;
        break;
      }
    }
    transfers = transfers.filter((transfer) => !removed.has(transfer.id));
    if (removed.size === finished.length) activityMessage = `Cleared ${removed.size} finished transfer${removed.size === 1 ? '' : 's'}`;
    if (nativeReady) await refreshLocalLibrary();
  }

  async function togglePause() {
    paused = !paused;
    if (nativeReady) {
      try { await invoke('set_downloads_paused', { paused }); activityMessage = paused ? 'All active downloads paused' : 'Downloads resumed'; }
      catch (error) { activityMessage = `Could not change download state: ${String(error)}`; }
    }
  }

  async function chooseNapstrFolder() {
    if (!nativeReady) { activityMessage = 'Folder selection is available in the packaged desktop app'; return; }
    try {
      const selectedPath = await open({ directory: true, multiple: false, title: 'Choose the folder Napstr uses for downloads and sharing', defaultPath: napstrFolder || undefined });
      if (!selectedPath || Array.isArray(selectedPath)) return;
      activityMessage = 'Indexing files and calculating SHA-256 hashes…';
      const report = await invoke<{ fileCount: number; totalBytes: number; errors: string[] }>('set_napstr_folder', { path: selectedPath });
      await refreshSnapshot();
      if (networkConnected) await invoke('publish_catalogue');
      activityMessage = `Indexed ${report.fileCount} file(s), ${readableSize(report.totalBytes)}${report.errors.length ? ` · ${report.errors.length} skipped` : ''}`;
    } catch (error) { activityMessage = `Folder selection failed: ${String(error)}`; }
  }

  async function openNapstrFolder() {
    if (!nativeReady) return;
    try { await invoke('open_napstr_folder'); }
    catch (error) { activityMessage = `Could not open Napstr folder: ${String(error)}`; }
  }

  async function rescanSharedFolder() {
    if (!nativeReady || rescanPending) return;
    rescanPending = true;
    activityMessage = 'Rescanning Napstr folder…';
    try {
      const report = await invoke<{ fileCount: number; totalBytes: number }>('rescan_napstr_folder');
      await refreshSnapshot();
      if (networkConnected) await invoke('publish_catalogue');
      activityMessage = `Indexed ${report.fileCount} file(s), ${readableSize(report.totalBytes)}`;
    } catch (error) {
      activityMessage = `Rescan failed: ${String(error)}`;
    } finally {
      rescanPending = false;
    }
  }

  async function persistSettings() {
    if (!nativeReady) return;
    try {
      applySnapshot(await invoke<Snapshot>('save_settings', { settings: { napstrFolder, nostrRelays, displayName, profileAbout, profilePicture, relaysOverTor } }));
      if (networkConnected) await invoke('publish_profile');
      activityMessage = networkConnected ? 'Settings saved and profile published' : 'Settings saved';
    } catch (error) { activityMessage = `Could not save settings: ${String(error)}`; }
  }

  function startBackupExport() {
    if (!nativeReady) { activityMessage = 'Account backup is available in the packaged desktop app'; return; }
    backupDialog = 'export';
    backupPassphrase = '';
    backupPassphraseRepeat = '';
    backupError = '';
  }

  async function startBackupImport() {
    if (!nativeReady) { activityMessage = 'Account restore is available in the packaged desktop app'; return; }
    const selectedPath = await open({ multiple: false, title: 'Choose your Napstr account backup', filters: [{ name: 'Napstr backup', extensions: ['ncryptsec', 'txt'] }] });
    if (!selectedPath || Array.isArray(selectedPath)) return;
    backupImportPath = selectedPath;
    backupDialog = 'import';
    backupPassphrase = '';
    backupRestoreNpub = '';
    backupAcknowledged = false;
    backupError = '';
  }

  function closeBackupDialog() {
    if (backupBusy) return;
    backupDialog = null;
    backupPassphrase = '';
    backupPassphraseRepeat = '';
    backupRestoreNpub = '';
    backupAcknowledged = false;
    backupError = '';
  }

  async function submitBackup() {
    backupError = '';
    if (backupDialog === 'export') {
      if (backupPassphrase.length < 8) { backupError = 'Use at least 8 characters.'; return; }
      if (backupPassphrase !== backupPassphraseRepeat) { backupError = 'The passphrases do not match.'; return; }
      const path = await save({ title: 'Save your account backup', defaultPath: 'napstr-account.ncryptsec' });
      if (!path) return;
      backupBusy = true;
      try {
        await invoke('export_identity_backup', { path, passphrase: backupPassphrase });
        backupBusy = false;
        closeBackupDialog();
        activityMessage = 'Backup saved. Keep the file and the passphrase safe — there is no reset.';
      } catch (error) { backupError = String(error); }
      finally { backupBusy = false; }
    } else if (backupDialog === 'import') {
      // Read-only step: prove the passphrase and name the account before anything is replaced.
      backupBusy = true;
      try {
        const preview = await invoke<{ restoredNpub: string; currentNpub: string; currentBackedUp: boolean }>('inspect_identity_backup', { path: backupImportPath, passphrase: backupPassphrase });
        backupRestoreNpub = preview.restoredNpub;
        backupCurrentNpub = preview.currentNpub;
        backupCurrentBackedUp = preview.currentBackedUp;
        backupDialog = 'import-confirm';
      } catch (error) { backupError = String(error); }
      finally { backupBusy = false; }
    }
  }

  async function loadArchivedIdentities() {
    if (!nativeReady) return;
    try { archivedIdentities = await invoke<ArchivedIdentity[]>('archived_identities'); }
    catch { archivedIdentities = []; }
  }

  async function adoptArchived(entry: ArchivedIdentity) {
    if (backupBusy) return;
    backupBusy = true;
    try {
      identityNpub = await invoke<string>('adopt_archived_identity', { keyringAccount: entry.keyringAccount });
      activityMessage = 'Switched back to the earlier account. The one it replaced was kept too.';
      await refreshSnapshot();
      await loadArchivedIdentities();
    } catch (error) { activityMessage = `Could not switch account: ${String(error)}`; }
    finally { backupBusy = false; }
  }

  async function confirmRestore() {
    if (backupBusy) return;
    backupBusy = true;
    backupError = '';
    try {
      const npub = await invoke<string>('import_identity_backup', { path: backupImportPath, passphrase: backupPassphrase });
      identityNpub = npub;
      closeBackupDialog();
      activityMessage = 'Account restored. The replaced account was kept on this computer under Previous accounts.';
      await refreshSnapshot();
      await loadArchivedIdentities();
    } catch (error) { backupError = String(error); }
    finally { backupBusy = false; }
  }

  const windowCommand = async (command: 'minimise_window' | 'toggle_maximise' | 'close_window') => {
    if (nativeReady) await invoke(command);
  };

  function beginWindowResize(event: PointerEvent, direction: WindowResizeDirection) {
    if (event.button !== 0) return;
    event.preventDefault();
    event.stopPropagation();
    getCurrentWindow().startResizeDragging(direction).catch(() => {});
  }

  function transferPaneMaximum() {
    return typeof window === 'undefined' ? 300 : Math.max(80, window.innerHeight - 395);
  }

  function setTransferPaneHeight(height: number, remember = false) {
    transferPaneHeight = Math.round(Math.min(transferPaneMaximum(), Math.max(48, height)));
    if (remember) window.localStorage.setItem('napstr-transfer-pane-height', String(transferPaneHeight));
  }

  function beginTransferResize(event: PointerEvent) {
    if (event.button !== 0) return;
    event.preventDefault();
    stopTransferResize();
    const startY = event.clientY;
    const startHeight = transferPaneHeight;
    const move = (next: PointerEvent) => setTransferPaneHeight(startHeight + startY - next.clientY);
    const stop = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', stop);
      window.removeEventListener('pointercancel', stop);
      document.body.classList.remove('resizing-transfer-pane');
      window.localStorage.setItem('napstr-transfer-pane-height', String(transferPaneHeight));
      stopTransferResize = () => {};
    };
    stopTransferResize = stop;
    document.body.classList.add('resizing-transfer-pane');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', stop);
    window.addEventListener('pointercancel', stop);
  }

  function resizeTransferWithKeyboard(event: KeyboardEvent) {
    if (event.key === 'ArrowUp') setTransferPaneHeight(transferPaneHeight + 20, true);
    else if (event.key === 'ArrowDown') setTransferPaneHeight(transferPaneHeight - 20, true);
    else if (event.key === 'Home') setTransferPaneHeight(48, true);
    else if (event.key === 'End') setTransferPaneHeight(transferPaneMaximum(), true);
    else return;
    event.preventDefault();
  }

  onMount(() => {
    desktopRuntime = '__TAURI_INTERNALS__' in window;
    if (!desktopRuntime) return;
    const savedPlayerMode = window.localStorage.getItem('napstr-player-mode');
    if (savedPlayerMode === 'single' || savedPlayerMode === 'folder' || savedPlayerMode === 'all' || savedPlayerMode === 'shuffle') playerMode = savedPlayerMode;
    const savedPlayerVolume = Number(window.localStorage.getItem('napstr-player-volume'));
    if (Number.isFinite(savedPlayerVolume) && savedPlayerVolume >= 0 && savedPlayerVolume <= 1) playerVolume = savedPlayerVolume;
    const savedTransferHeight = Number(window.localStorage.getItem('napstr-transfer-pane-height'));
    setTransferPaneHeight(Number.isFinite(savedTransferHeight) && savedTransferHeight > 0 ? savedTransferHeight : window.innerHeight < 700 ? 94 : 119);
    const clampTransferPane = () => setTransferPaneHeight(transferPaneHeight);
    window.addEventListener('resize', clampTransferPane);
    const snapshotReady = refreshSnapshot();
    void snapshotReady.then(connectNetwork);
    void loadArchivedIdentities();
    void getVersion()
      .then(async (version) => {
        appVersion = version;
        await snapshotReady.catch(() => undefined);
        return checkForNewRelease();
      })
      .catch(() => {
        appVersion = 'unknown';
      });
    let destroyed = false;
    const chatUnlisteners: UnlistenFn[] = [];
    void listen<string>('napstr-public-chat', ({ payload: topic }) => {
      if (topic === 'napstr-trollbox') void refreshTrollbox();
      const fileId = selected?.fileId?.toLowerCase();
      if (fileId && topic === `napstr-${fileId}`) void refreshTrackDiscussion(fileId);
    }).then((unlisten) => {
      if (destroyed) unlisten();
      else chatUnlisteners.push(unlisten);
    });
    const updateClock = () => {
      clock = new Intl.DateTimeFormat('en-GB', { hour: '2-digit', minute: '2-digit' }).format(new Date());
    };
    updateClock();
    const clockTimer = window.setInterval(updateClock, 30000);
    let lastWakeTick = Date.now();
    let lastWakeRecovery = 0;
    const detectWake = () => {
      const now = Date.now();
      const elapsed = now - lastWakeTick;
      lastWakeTick = now;
      if (elapsed > 20000 && now - lastWakeRecovery > 15000) {
        lastWakeRecovery = now;
        window.setTimeout(() => void recoverAfterSleep(), 1200);
      }
    };
    const foregrounded = () => {
      if (!document.hidden) detectWake();
    };
    window.addEventListener('focus', foregrounded);
    document.addEventListener('visibilitychange', foregrounded);
    const wakeTimer = window.setInterval(detectWake, 5000);
    let networkPollPending = false;
    const networkTimer = window.setInterval(async () => {
      if (!nativeReady || networkPollPending) return;
      networkPollPending = true;
      try {
        const status = await invoke<NetworkStatus>('network_status');
        const previousTorError = torError;
        const wasConnected = networkConnected;
        applyNetworkStatus(status);
        if (status.torError && status.torError !== previousTorError) {
          activityMessage = `Tor failed: ${status.torError} · click the connection panel to retry`;
        } else if (!wasConnected && status.connected) {
          activityMessage = 'Nostr reconnected · refreshing the catalogue';
          void search();
          if (activeView === 'Trollbox') void refreshTrollbox();
        } else if (!status.connected && !networkConnectPending) {
          void connectNetwork();
        }
      } catch { /* the next health poll retries */ }
      finally { networkPollPending = false; }
    }, 5000);
    let transferPollPending = false;
    const transferTimer = window.setInterval(async () => {
      if (!nativeReady || transferPollPending) return;
      transferPollPending = true;
      try {
        const items = await invoke<NativeTransfer[]>('get_transfers');
        try {
          const stats = await invoke<SeedingStat[]>('get_seeding_stats');
          seedingStats = Object.fromEntries(stats.map((stat) => [stat.fileId, stat]));
          sharedFiles = sharedFiles.map((file) => ({ ...file, peers: seedingStats[file.fileId]?.activeGrants ?? 0, delivered: seedingStats[file.fileId]?.delivered ?? 0, otherSeeders: seedingStats[file.fileId]?.otherSeeders ?? 0 }));
        } catch { /* seeding stats are cosmetic; the next poll retries */ }
        const previouslyComplete = new Set(transfers.filter(isCompleteTransfer).map((transfer) => transfer.fileId));
        const updated = mapTransfers(items);
        const newlyComplete = updated.filter((transfer) => isCompleteTransfer(transfer) && !previouslyComplete.has(transfer.fileId));
        const vanishedActive = transfers.filter((transfer) => !startingDownloads.has(transfer.fileId) && isActiveTransfer(transfer) && !updated.some((item) => item.id === transfer.id));
        const optimistic = transfers.filter((transfer) => startingDownloads.has(transfer.fileId) && !updated.some((item) => item.fileId === transfer.fileId));
        transfers = [...optimistic, ...updated];
        if (newlyComplete.length || vanishedActive.length) {
          await refreshLocalLibrary();
          const latest = newlyComplete[0] ?? vanishedActive[0];
          activityMessage = `${latest.name} downloaded, verified, and ready to play`;
        }
      } catch { /* the next transfer poll retries */ }
      finally { transferPollPending = false; }
    }, 1000);
    let libraryPollPending = false;
    const libraryTimer = window.setInterval(async () => {
      if (!nativeReady || libraryPollPending) return;
      libraryPollPending = true;
      try { await refreshLocalLibrary(); }
      finally { libraryPollPending = false; }
    }, 2000);
    const playerTimer = window.setInterval(() => {
      if (!nativeReady || !currentTrack || playerLoading) return;
      invoke<PlaybackStatus>('audio_status').then((status) => {
        const naturallyEnded = status.fileId === currentTrack?.fileId && status.ended && !playerEnded;
        applyPlaybackStatus(status);
        if (naturallyEnded) void playerTrackEnded();
      }).catch(() => {});
    }, 250);
    return () => {
      destroyed = true;
      chatUnlisteners.forEach((unlisten) => unlisten());
      clearInterval(clockTimer);
      clearInterval(wakeTimer);
      clearInterval(networkTimer);
      clearInterval(transferTimer);
      clearInterval(libraryTimer);
      clearInterval(playerTimer);
      window.removeEventListener('resize', clampTransferPane);
      window.removeEventListener('focus', foregrounded);
      document.removeEventListener('visibilitychange', foregrounded);
      stopTransferResize();
      if (nativeReady && currentTrack) invoke<PlaybackStatus>('stop_audio').catch(() => {});
    };
  });
</script>

<svelte:head><title>Napstr - own your music again</title></svelte:head>

{#if desktopRuntime}
<main class="desktop">
  <section class="app-window" style={`--transfer-height: ${transferPaneHeight}px`} aria-label="Napstr application window">
    <button class="window-resize-handle resize-n" aria-label="Resize window from top" onpointerdown={(event) => beginWindowResize(event, 'North')}></button>
    <button class="window-resize-handle resize-e" aria-label="Resize window from right" onpointerdown={(event) => beginWindowResize(event, 'East')}></button>
    <button class="window-resize-handle resize-s" aria-label="Resize window from bottom" onpointerdown={(event) => beginWindowResize(event, 'South')}></button>
    <button class="window-resize-handle resize-w" aria-label="Resize window from left" onpointerdown={(event) => beginWindowResize(event, 'West')}></button>
    <button class="window-resize-handle resize-ne" aria-label="Resize window from top right" onpointerdown={(event) => beginWindowResize(event, 'NorthEast')}></button>
    <button class="window-resize-handle resize-se" aria-label="Resize window from bottom right" onpointerdown={(event) => beginWindowResize(event, 'SouthEast')}></button>
    <button class="window-resize-handle resize-sw" aria-label="Resize window from bottom left" onpointerdown={(event) => beginWindowResize(event, 'SouthWest')}></button>
    <button class="window-resize-handle resize-nw" aria-label="Resize window from top left" onpointerdown={(event) => beginWindowResize(event, 'NorthWest')}></button>

    <header class="titlebar" data-tauri-drag-region>
      <div class="title-left"><span class="app-icon"><img src="/napstr-logo.png" alt="" /></span><span>Napstr - own your music again</span></div>
      <div class="window-controls" aria-hidden="true">
        <button tabindex="-1" onclick={() => windowCommand('minimise_window')}>_</button><button tabindex="-1" onclick={() => windowCommand('toggle_maximise')}>□</button><button tabindex="-1" onclick={() => windowCommand('close_window')}>×</button>
      </div>
    </header>

    <div class="toolbar">
      <div class="toolbar-brand" title="Napstr home">
        <img src="/napstr-logo.png" alt="Napstr" />
      </div>
      <div class="toolbar-separator"></div>
      {#each views as view}
        <button class:active={activeView === view.label} class="tool-button" onclick={() => activateView(view.label)}>
          <span class="tool-icon icon-{view.label.toLowerCase()}">{view.icon}</span>
          <span>{view.label}</span>
        </button>
      {/each}
      <div class="toolbar-spacer"></div>
      {#if newRelease}
        <button class="release-button" onclick={openNewRelease} title={`Open Napstr ${newRelease.version} on GitHub`}>
          <span class="release-arrow">⇧</span>
          <span><strong>New release</strong><small>{newRelease.version} available</small></span>
        </button>
      {/if}
      <button class="connection-box" onclick={connectNetwork} title={torError || networkError || 'Reconnect Nostr and Tor'}>
        <span class="connection-status"><i class:amber={!networkConnected} class="led"></i><strong>{networkConnected ? 'Connected' : 'Connect'}</strong></span>
        <span class="connection-status"><i class:amber={!torRunning} class:error={Boolean(torError)} class="led"></i><strong>{torStatusLabel()}</strong></span>
      </button>
      <button class="tool-button help-button" onclick={() => (aboutOpen = true)}><span class="tool-icon">?</span><span>About</span></button>
    </div>

    <div class="network-strip">
      <span class="network-pulse">▥</span>
      <span>{activityMessage}</span>
      <span class="strip-right">{displayName} <i class:amber={!nativeReady} class="led"></i></span>
    </div>

    <section class="player-bar" aria-label="Napstr audio player">
      <div class="player-display">
        <span class:playing={playerPlaying} class="player-led">{playerLoading ? '···' : playerPlaying ? '▶' : '■'}</span>
        <div><strong>{currentTrack?.name ?? 'No track selected'}</strong><small>{currentTrack ? `${currentTrack.artist || 'Unknown artist'} · ${folderName(currentTrack.folder)}` : 'Choose a local song to begin'}</small></div>
      </div>
      <div class="player-controls">
        <button onclick={previousPlayerTrack} disabled={!currentTrack || playerLoading} title="Previous track">|◀</button>
        <button class="player-primary" onclick={togglePlayer} disabled={playerLoading} title={playerPlaying ? 'Pause' : 'Play'}>{playerLoading ? '…' : playerPlaying ? 'Ⅱ' : '▶'}</button>
        <button onclick={stopPlayer} disabled={!currentTrack || playerLoading} title="Stop">■</button>
        <button onclick={nextPlayerTrack} disabled={playerLoading || playerQueueIndex < 0 || playerQueueIndex + 1 >= playerQueue.length} title="Next track">▶|</button>
      </div>
      <div class="player-seek">
        <input aria-label="Track position" type="range" min="0" max={Math.max(0, playerDuration || 0)} step="0.1" value={playerCurrentTime} oninput={seekPlayer} disabled={!currentTrack} />
        <span>{formatPlayerTime(playerCurrentTime)} / {formatPlayerTime(playerDuration)}</span>
      </div>
      <label class="player-mode">After track
        <select bind:value={playerMode} onchange={changePlayerMode}>
          <option value="single">Stop</option>
          <option value="folder">Play folder</option>
          <option value="all">Play all</option>
          <option value="shuffle">Shuffle</option>
        </select>
      </label>
      <label class="player-volume">Vol <input aria-label="Volume" type="range" min="0" max="1" step="0.05" value={playerVolume} oninput={changePlayerVolume} /></label>
    </section>

    <div class="workspace">
      {#if activeView === 'Search'}
        <section class="panel search-panel">
          <div class="panel-title"><span></span><b>Search the Napstr network</b><span></span></div>
          <form class="search-form" onsubmit={(e) => { e.preventDefault(); search(); }}>
            <label for="search-query">Search:</label>
            <input id="search-query" bind:value={query} placeholder="punk, rock, jazz, audiobook" />
            <label for="format">File type:</label>
            <select id="format" bind:value={format}><option>Audio only</option><option>FLAC</option><option>MP3</option><option>WAV</option><option>OGG</option><option>OPUS</option></select>
            <button class="classic-button primary search-button" type="submit" disabled={searchAction !== null} aria-busy={searchAction === 'search'}>
              {#if searchAction === 'search'}<span class="search-spinner" aria-hidden="true"></span>{/if}
              {searchAction === 'search' ? 'Searching' : 'Search'}
            </button>
            <button class="classic-button surprise-button" type="button" onclick={surpriseMe} disabled={searchAction !== null || !networkConnected} aria-busy={searchAction === 'surprise'}>
              {#if searchAction === 'surprise'}<span class="search-spinner" aria-hidden="true"></span>{/if}
              {searchAction === 'surprise' ? 'Choosing…' : 'Surprise me'}
            </button>
          </form>
          <button class="advanced-toggle" onclick={() => (advanced = !advanced)}><span>{advanced ? '▼' : '▶'}</span> {advanced ? 'Hide' : 'Show'} advanced search options</button>
          {#if advanced}
            <div class="advanced-row"><label>Minimum seeders: <input type="number" bind:value={minimumSources} min="1" /></label><label>Maximum size: <input bind:value={maximumSize} placeholder="e.g. 2 GB" /></label><label><input type="checkbox" checked disabled /> Online seeders only</label></div>
          {/if}
        </section>

        <div class="split-content">
          <section class="results-pane" aria-label="Search results">
            <div class="section-caption"><span>Search results for “{searchedQuery}”</span><small>{results.length} track{results.length === 1 ? '' : 's'} found</small></div>
            <div class="table-wrap">
              <table class="file-table">
                <thead><tr><th class="name-col"><button class="sort-header" onclick={() => toggleSort('name')}>Name{sortIndicator('name')}</button></th><th><button class="sort-header" onclick={() => toggleSort('format')}>Type{sortIndicator('format')}</button></th><th class="number"><button class="sort-header" onclick={() => toggleSort('bytes')}>Size{sortIndicator('bytes')}</button></th><th class="number"><button class="sort-header" onclick={() => toggleSort('sources')}>Seeders{sortIndicator('sources')}</button></th><th>Line speed</th><th>Length</th></tr></thead>
                <tbody>
                  {#each paginatedResults() as item}
                    <tr class:selected={selected?.id === item.id} onclick={() => selectResult(item)} ondblclick={activateSelected}>
                      <td><span class="file-icon">▶</span>{item.name}</td><td>{item.format}</td><td class="number">{item.size}</td><td class="number"><span class="source-dot"></span>{item.sources}</td><td>{item.speed}</td><td>{item.length}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
              {#if results.length === 0}
                <p class="empty-state">{networkConnected ? 'Nothing found — try fewer or different words.' : nativeReady ? 'You are only seeing your own files. Press “Connect” at the top right to search everyone’s shared music.' : 'Starting up…'}</p>
              {/if}
            </div>
            <div class="results-pager">
              <button onclick={() => changeResultPage(resultPage - 1)} disabled={resultPage === 0}>◀ Previous</button>
              <span>{resultRange()} of {results.length} · Page {resultPage + 1} of {resultPageCount()}</span>
              <button onclick={() => changeResultPage(resultPage + 1)} disabled={resultPage + 1 >= resultPageCount()}>Next ▶</button>
            </div>
          </section>

          <aside class="details-pane">
            <div class="section-caption"><span>File details</span></div>
            {#if selected}
              <div class="selected-file">
                <div class="large-file-icon">▶</div>
                <div><strong>{selected.name}</strong><span>{selected.format} · {selected.size} · {selected.length}</span><small>File ID: {selected.fileId}</small></div>
              </div>
              {#if selected.tags}<div class="file-metadata"><small>Tags: {selected.tags}</small></div>{/if}
              <fieldset><legend>Seeders</legend>
                <div class="sources-list">
                  {#if !isLocalFile(selected.fileId)}
                    {#each (selected.sourceDetails ?? []).slice(0, VISIBLE_SEEDER_LIMIT) as source, index}
                      <button class:selected-source={selectedSource === index} class="source-row" onclick={() => (selectedSource = index)}><span class="user-icon">☺</span><b>{source.displayName}</b><small>{source.npub.slice(0, 12)}…</small><span class="online"><i></i> Seeding</span></button>
                    {/each}
                  {:else}
                    <div><span class="user-icon">☺</span><b>This computer</b><small>Local</small><span class="online"><i></i> Ready</span></div>
                  {/if}
                </div>
              </fieldset>
              <div class="detail-actions">{#if !isLocalFile(selected.fileId)}<button class="classic-button primary" disabled={startingDownloads.has(selected.fileId)} onclick={startDownload}>{startingDownloads.has(selected.fileId) ? '… Requesting' : '⇩ Download'}</button><button class="classic-button" onclick={() => (sourceProfile = selected?.sourceDetails?.[selectedSource] ?? null)}>View profile</button>{:else}<button class="classic-button primary" onclick={playSelectedAudio}>▶ Play</button><button class="classic-button" onclick={openNapstrFolder}>Open folder</button>{/if}</div>
              {#if !isLocalFile(selected.fileId)}<div class="detail-actions moderation-actions"><button class="classic-button" onclick={blockSelectedFile}>Block file</button><button class="classic-button" onclick={blockSelectedUser}>Block user</button></div>{/if}
              {#if !isLocalFile(selected.fileId)}<p class="privacy-note"><span>♜</span> Transfer will use the seeder’s private, app-session Tor onion service.</p>{:else}<p class="privacy-note"><span>♬</span> Downloaded and verified · ready to play from your Napstr folder.</p>{/if}
              <section class="track-discussion" aria-label={`Discussion for ${selected.name}`}>
                <div class="track-discussion-title"><b>Track discussion</b><small>Public · Nostr</small></div>
                <div class="track-discussion-log" bind:this={trackDiscussionLog} aria-live="polite">
                  {#if trackDiscussionLoading}<p class="trollbox-notice">Loading comments…</p>{/if}
                  {#if !trackDiscussionLoading && trackDiscussionMessages.length === 0 && !trackDiscussionError}<p class="trollbox-notice">No comments yet.</p>{/if}
                  {#each trackDiscussionMessages as message (message.eventId)}
                    <div class="trollbox-message"><button class="trollbox-name" style:color={chatNameColor(message.npub)} title={`${message.npub} · click to block`} disabled={message.npub === identityNpub} onclick={() => blockTrollboxUser(message)}>{message.displayName}:</button><span>{message.content}</span></div>
                  {/each}
                </div>
                {#if trackDiscussionError}<div class="track-discussion-error">{trackDiscussionError}</div>{/if}
                <div class="track-discussion-compose">
                  <input bind:value={trackDiscussionDraft} maxlength="500" autocomplete="off" placeholder={networkConnected ? 'Comment on this track…' : 'Connect to Nostr to comment'} disabled={!networkConnected || trackDiscussionSending} aria-label="Track discussion comment" onkeydown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void sendTrackDiscussionMessage(); } }} />
                  <button class="classic-button primary" type="button" disabled={!networkConnected || trackDiscussionSending || !trackDiscussionDraft.trim()} onclick={() => void sendTrackDiscussionMessage()}>{trackDiscussionSending ? '…' : 'Send'}</button>
                </div>
              </section>
            {:else}<p class="empty-state">{networkConnected ? 'Select a result to see who is sharing it right now.' : 'Not connected yet — press “Connect” at the top right to see who is sharing music.'}</p>{/if}
          </aside>
        </div>
      {:else if activeView === 'Downloads'}
        <section class="full-panel downloads-view">
          <div class="panel-title"><span></span><b>Download Manager</b><span></span></div>
          <div class="actionbar"><button class="classic-button" onclick={togglePause}>{paused ? '▶ Resume all' : 'Ⅱ Pause all'}</button><button class="classic-button" onclick={openNapstrFolder}>Open Napstr folder</button><button class="classic-button" onclick={clearFinishedTransfers} disabled={!transfers.some(isFinishedTransfer)}>Clear finished</button><div class="spacer"></div><span>{transfers.filter(isActiveTransfer).length} active · {transfers.filter(isCompleteTransfer).length} ready to play</span></div>
          <div class="download-queue">
            <table class="file-table download-table"><thead><tr><th>Download order</th><th>Progress</th><th>Size</th><th>Speed</th><th>Status</th><th></th></tr></thead><tbody>
              {#each transfers as transfer}
                <tr class:transfer-complete={isCompleteTransfer(transfer)} ondblclick={() => { if (isCompleteTransfer(transfer)) playAudio(transfer.fileId, transfer.name, playerMode, 'downloads'); }}><td><span class="download-arrow">{isCompleteTransfer(transfer) ? '▶' : '⇩'}</span>{transfer.name}</td><td><div class="progress"><span style={`width:${transfer.progress}%`}></span><b>{Math.round(transfer.progress)}%</b></div></td><td>{transfer.size}</td><td>{isCompleteTransfer(transfer) ? 'Local' : transfer.speed}</td><td>{isCompleteTransfer(transfer) ? 'Ready to play' : transfer.status}</td><td class="transfer-actions">{#if isCompleteTransfer(transfer)}<button class="classic-button transfer-play" onclick={(event) => { event.stopPropagation(); playAudio(transfer.fileId, transfer.name, playerMode, 'downloads'); }} title="Play verified audio">▶ Play</button>{/if}<button class="tiny-button" onclick={(event) => { event.stopPropagation(); removeTransfer(transfer.id); }} title="Remove from this list">×</button></td></tr>
              {/each}
            </tbody></table>
            {#if transfers.length === 0}<p class="empty-state compact">There are no downloads in the queue.</p>{/if}
          </div>
          <div class="panel-title"><span></span><b>Track Tags</b><span></span></div>
          <div class="tag-editor">
            <b>{selectedTagFile?.filename ?? 'Select a local track below'}</b>
            <input bind:value={tagDraft} disabled={!selectedTagFile || tagSaving} maxlength="256" placeholder="punk, live, audiobook" onkeydown={(event) => { if (event.key === 'Enter') saveTags(); }} />
            <button class="classic-button primary" onclick={saveTags} disabled={!selectedTagFile || tagSaving}>{tagSaving ? 'Saving…' : 'Save tags'}</button>
            <small>Comma-separated · published with your signed catalogue</small>
          </div>
          <div class="tag-library">
            <table class="file-table tags-table"><thead><tr><th>Name</th><th>Folder</th><th>Tags</th></tr></thead><tbody>
              {#each sharedFiles as file}
                <tr class:selected={selectedTagFile?.fileId === file.fileId} onclick={() => selectTagFile(file)} ondblclick={() => playAudio(file.fileId, file.filename, playerMode, 'downloads')}><td><button type="button" class="file-icon file-play-button" title={`Play ${file.filename}`} aria-label={`Play ${file.filename}`} onclick={(event) => { event.stopPropagation(); selectTagFile(file); playAudio(file.fileId, file.filename, playerMode, 'downloads'); }}>▶</button>{file.filename}</td><td>{folderName(file.folder)}</td><td>{file.tags || '—'}</td></tr>
              {/each}
            </tbody></table>
            {#if sharedFiles.length === 0}<p class="empty-state compact">Downloaded and shared tracks will appear here.</p>{/if}
          </div>
        </section>
      {:else if activeView === 'Shared'}
        <section class="full-panel">
          <div class="panel-title"><span></span><b>My Shared Files</b><span></span></div>
          <div class="actionbar"><button class="classic-button" onclick={rescanSharedFolder} disabled={rescanPending}>{rescanPending ? '… Rescanning' : '↻ Rescan'}</button><button class="classic-button" onclick={openNapstrFolder}>Open folder</button><button class="classic-button" onclick={playSelectedSharedAudio} disabled={!selectedShared}>▶ Play</button><button class="classic-button" onclick={playSelectedFolder} disabled={!selectedShared}>▶ Play folder</button><button class="classic-button primary" onclick={playAllSongs} disabled={!sharedFiles.length}>▶ Play all</button><div class="spacer"></div><span>Sharing {sharedFiles.length} files · {readableSize(indexedBytes)}</span></div>
          <div class="folder-path"><b>Napstr folder:</b><input value={napstrFolder || 'No folder selected'} readonly /><button class="classic-button" onclick={chooseNapstrFolder}>Browse…</button></div>
          <div class="library-filter"><label>View folder: <select bind:value={libraryFolderView}><option value="*">All folders</option>{#each libraryFolders() as folder}<option value={folder}>{folderName(folder)}</option>{/each}</select></label><span>{visibleSharedFiles().length} song{visibleSharedFiles().length === 1 ? '' : 's'} shown</span></div>
          <table class="file-table shared-table"><thead><tr><th>Name</th><th>Folder</th><th>Size</th><th>Catalogue</th><th>Uploads</th></tr></thead><tbody>{#each visibleSharedFiles() as file}<tr class:selected={selectedShared?.fileId === file.fileId} onclick={() => (selectedShared = { ...file })} ondblclick={() => playAudio(file.fileId, file.name, playerMode, 'shared')}><td><span class="file-icon">▶</span>{file.name}</td><td>{folderName(file.folder)}</td><td>{file.readableSize}</td><td><span class:amber={!networkConnected} class="led"></span>{networkConnected ? `Published${file.otherSeeders ? ` · +${file.otherSeeders} others` : ''}` : 'Indexed'}</td><td>{file.delivered || file.peers ? `${file.delivered} delivered${file.peers ? ` · ${file.peers} active` : ''}` : '—'}</td></tr>{/each}</tbody></table>
          <p class="privacy-note wide"><span>♜</span> Only validated MP3, FLAC, WAV, Ogg Vorbis, and Opus audio is indexed recursively. Subfolders become player folders; folder names remain local and are not published. Embedded cover artwork is allowed.</p>
          <p class="privacy-note wide"><span>i</span> Removing a file from your folder removes only your own listing. Copies that other people already share stay available — “+N others” shows who else is offering the same file right now.</p>
        </section>
      {:else if activeView === 'Trollbox'}
        <section class="full-panel trollbox-view">
          <div class="panel-title"><span></span><b>Napstr Trollbox</b><span></span></div>
          <div class="trollbox-status"><span><i class:amber={!networkConnected} class="led"></i> Public Nostr chat: <b>#napstr-trollbox</b></span><small>NIP-C7 messages are public and signed by your Napstr Nostr identity.</small></div>
          <div class="trollbox-log" bind:this={trollboxLog} aria-live="polite" aria-label="Napstr public chat messages">
            {#if trollboxLoading}<p class="trollbox-notice">Connecting to the trollbox…</p>{/if}
            {#if !trollboxLoading && trollboxMessages.length === 0 && !trollboxError}<p class="trollbox-notice">No messages yet. Say hello.</p>{/if}
            {#each trollboxMessages as message (message.eventId)}
              <div class="trollbox-message"><button class="trollbox-name" style:color={chatNameColor(message.npub)} title={`${message.npub} · click to block`} disabled={message.npub === identityNpub} onclick={() => blockTrollboxUser(message)}>{message.displayName}:</button><span>{message.content}</span></div>
            {/each}
          </div>
          {#if trollboxError}<div class="trollbox-error">{trollboxError}</div>{/if}
          <div class="trollbox-compose">
            <input bind:value={trollboxDraft} maxlength="500" autocomplete="off" placeholder={networkConnected ? 'Type a public message…' : 'Connect to Nostr to chat'} disabled={!networkConnected || trollboxSending} aria-label="Trollbox message" onkeydown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void sendTrollboxMessage(); } }} />
            <button class="classic-button primary" type="button" disabled={!networkConnected || trollboxSending || !trollboxDraft.trim()} onclick={() => void sendTrollboxMessage()}>{trollboxSending ? 'Sending…' : 'Send'}</button>
          </div>
        </section>
      {:else if activeView === 'Profile'}
        <section class="full-panel profile-view">
          <div class="panel-title"><span></span><b>Napstr Profile</b><span></span></div>
          <div class="profile-card"><div class="avatar"><img src="/napstr-logo.png" alt="Napstr mascot" /></div><div><h2>{displayName}</h2><p>Your dedicated Napstr Nostr identity.</p><code>{identityNpub || 'Connect to create identity'}</code><div class="profile-stats"><span><b>{sharedFiles.length}</b> shared files</span><span><b>{transfers.length}</b> transfers</span><span><b>{networkConnected ? 'Nostr online' : 'Offline'}</b></span></div></div></div>
          <fieldset class="edit-profile"><legend>Profile</legend><label>Display name <input bind:value={displayName} /></label><label>About <input bind:value={profileAbout} /></label><label>Picture URL <input bind:value={profilePicture} placeholder="https://…" /></label><button class="classic-button primary" onclick={persistSettings}>Save profile</button><div class="backup-actions"><button class="classic-button" onclick={startBackupExport}>Back up account…</button><button class="classic-button" onclick={() => void startBackupImport()}>Restore backup…</button></div></fieldset>
          {#if archivedIdentities.length}
            <fieldset class="edit-profile"><legend>Previous accounts on this computer</legend>
              <p>Accounts replaced by a restore are kept here so a mistaken restore can be undone. They live in this computer's keychain only.</p>
              <ul class="archived-identities">
                {#each archivedIdentities as entry (entry.keyringAccount)}
                  <li><code>{entry.npub}</code><span>replaced {new Date(entry.archivedAt).toLocaleDateString()}</span><button class="classic-button" disabled={backupBusy} onclick={() => void adoptArchived(entry)}>Switch back</button></li>
                {/each}
              </ul>
            </fieldset>
          {/if}
          <div class="public-panel">
            <fieldset><legend>What everyone can see</legend>
              <ul>
                <li>Your account key, display name, about text, and picture link</li>
                <li>Names, sizes, formats, and tags of every file you share — downloads you keep in the Napstr folder are shared too</li>
                <li>That you are online while the app is sharing</li>
                <li>Every chat message, signed by this account</li>
              </ul>
            </fieldset>
            <fieldset><legend>What is never published</legend>
              <ul>
                <li>Your internet address — transfers run through Tor</li>
                <li>The music itself, until someone you granted downloads it</li>
                <li>Download requests and transfer credentials — encrypted end to end</li>
                <li>Your folder names and anything outside the Napstr folder</li>
              </ul>
            </fieldset>
          </div>
          <p class="privacy-note wide"><span>i</span> Everything public above is tied to this one account: your chat, your shared music, and your profile can be linked to each other by anyone.</p>
        </section>
      {:else}
        <section class="full-panel settings-view">
          <div class="panel-title"><span></span><b>Napstr Settings</b><span></span></div>
          <fieldset><legend>Network</legend><label><input type="checkbox" checked disabled /> Connect automatically at startup</label><label><input type="checkbox" checked disabled /> Never allow direct-IP file transfer</label><label><input type="checkbox" bind:checked={relaysOverTor} /> Extra privacy: reach the music network only through Tor (connecting takes longer; applies the next time you connect)</label><label>Nostr relays <input bind:value={nostrRelays} /></label><label>Tor <input value="Bundled, managed automatically" readonly /></label></fieldset>
          <fieldset><legend>Files</legend><label>Downloads and shared audio <input value={napstrFolder} readonly /><button class="classic-button" onclick={chooseNapstrFolder}>Browse…</button></label><label>Transfer mode <select disabled><option>Whole file</option></select></label><label><input type="checkbox" checked disabled /> Downloaded audio is automatically shared</label><label><input type="checkbox" checked disabled /> Verify the complete file with SHA-256</label></fieldset>
          <div class="settings-actions"><button class="classic-button primary" onclick={persistSettings}>OK</button><button class="classic-button" onclick={refreshSnapshot}>Cancel</button><button class="classic-button" onclick={persistSettings}>Apply</button></div>
        </section>
      {/if}
    </div>

    <section class="transfer-dock">
      <button
        type="button"
        class="dock-resizer"
        aria-label="Resize Transfer Manager"
        title="Drag to resize Transfer Manager · double-click to reset"
        onpointerdown={beginTransferResize}
        onkeydown={resizeTransferWithKeyboard}
        ondblclick={() => setTransferPaneHeight(window.innerHeight < 700 ? 94 : 119, true)}
      ></button>
      <div class="dock-title"><span></span><b>Transfer Manager</b><span></span><button class="dock-clear" onclick={clearFinishedTransfers} disabled={!transfers.some(isFinishedTransfer)}>Clear finished</button><button onclick={() => (activeView = 'Downloads')} title="Open Download Manager">□</button></div>
      <div class="mini-transfers">
        {#each transfers as transfer}
          <div class:transfer-complete={isCompleteTransfer(transfer)} class="mini-row">{#if isCompleteTransfer(transfer)}<button class="mini-play" onclick={() => playAudio(transfer.fileId, transfer.name, playerMode, 'downloads')} title="Play verified audio">▶</button>{:else}<span class="download-arrow">⇩</span>{/if}<span class="mini-name">{transfer.name}</span><div class="progress"><span style={`width:${transfer.progress}%`}></span></div><span>{transfer.size}</span><span>{isCompleteTransfer(transfer) ? 'Ready' : transfer.speed}</span></div>
        {/each}
      </div>
    </section>

    <footer class="statusbar"><span>{activityMessage}</span><span><i class:amber={!networkConnected} class="led"></i> Nostr {networkConnected ? 'online' : 'offline'}</span><span title={torError}>♜ Tor: {torRunning ? 'ready' : torError ? 'failed' : torStarting && torProgress > 0 ? `${torProgress}%` : 'starting'}</span><span class="status-clock">{clock}</span></footer>
  </section>

  {#if aboutOpen}
    <div class="modal-backdrop" role="presentation" onclick={() => (aboutOpen = false)}>
      <dialog class="dialog" open aria-label="About Napstr" onclick={(e) => e.stopPropagation()} onkeydown={(e) => { if (e.key === 'Escape') aboutOpen = false; }}>
        <header class="titlebar"><div class="title-left"><span class="app-icon"><img src="/napstr-logo.png" alt="" /></span><span>About Napstr</span></div><div class="window-controls"><button onclick={() => (aboutOpen = false)}>×</button></div></header>
        <div class="dialog-body"><div class="about-logo"><img src="/napstr-logo.png" alt="" /></div><div><h2>Napstr</h2><p>Version {appVersion}</p><p>Public discovery over Nostr.<br />Private verified transfers over Tor.</p></div></div>
        <div class="dialog-actions"><button class="classic-button primary" onclick={() => (aboutOpen = false)}>OK</button></div>
      </dialog>
    </div>
  {/if}

  {#if sourceProfile}
    <div class="modal-backdrop" role="presentation" onclick={() => (sourceProfile = null)}>
      <dialog class="dialog" open aria-label="Napstr public profile" onclick={(e) => e.stopPropagation()}>
        <header class="titlebar"><div class="title-left"><span class="app-icon"><img src="/napstr-logo.png" alt="" /></span><span>Public Napstr Profile</span></div><div class="window-controls"><button onclick={() => (sourceProfile = null)}>×</button></div></header>
        <div class="dialog-body"><div class="about-logo">☺</div><div><h2>{sourceProfile.displayName}</h2><p>{sourceProfile.about || 'No profile description published.'}</p><code>{sourceProfile.npub}</code></div></div>
        <div class="dialog-actions">{#if networkConnected}<button class="classic-button primary" onclick={() => sourceProfile && showSourceCatalogue(sourceProfile)}>Show their shared music</button>{/if}<button class="classic-button" onclick={() => (sourceProfile = null)}>OK</button></div>
      </dialog>
    </div>
  {/if}

  {#if backupDialog}
    <div class="modal-backdrop" role="presentation" onclick={closeBackupDialog}>
      <dialog class="dialog confirm-dialog" open aria-label={backupDialog === 'export' ? 'Back up account' : backupDialog === 'import-confirm' ? 'Confirm account replacement' : 'Restore account'} onclick={(e) => e.stopPropagation()} onkeydown={(e) => { if (e.key === 'Escape') closeBackupDialog(); }}>
        <header class="titlebar"><div class="title-left"><span class="app-icon">🔑</span><span>{backupDialog === 'export' ? 'Back up account' : backupDialog === 'import-confirm' ? 'Replace this account?' : 'Restore account'}</span></div><div class="window-controls"><button disabled={backupBusy} onclick={closeBackupDialog}>×</button></div></header>
        <div class="dialog-body"><div class="backup-body">
          {#if backupDialog === 'export'}
            <p>Choose a passphrase for the backup file. The file is useless without it.</p>
            <label>Passphrase <input type="password" bind:value={backupPassphrase} minlength="8" autocomplete="new-password" /></label>
            <label>Repeat <input type="password" bind:value={backupPassphraseRepeat} autocomplete="new-password" /></label>
            <p class="backup-warning">There is no reset. If you lose the file or the passphrase, this account cannot be recovered — by anyone.</p>
          {:else if backupDialog === 'import'}
            <p>Enter the passphrase for this backup file. Nothing is replaced yet — you will see which account it holds before anything changes.</p>
            <label>Passphrase <input type="password" bind:value={backupPassphrase} autocomplete="current-password" /></label>
          {:else}
            {#if backupCurrentNpub && backupCurrentNpub !== backupRestoreNpub}
              <p>Replacing: <code>{backupCurrentNpub}</code></p>
              <p>Restoring: <code>{backupRestoreNpub}</code></p>
              {#if backupCurrentBackedUp}
                <p>The account being replaced has its own backup file, and a copy also stays on this computer under Previous accounts.</p>
                <label class="backup-ack"><input type="checkbox" bind:checked={backupAcknowledged} /> I understand that this account stops being the one Napstr uses.</label>
              {:else}
                <p class="backup-warning">The account being replaced has never been saved to a backup file. Napstr will keep a copy on this computer under Previous accounts, but that copy is all that will exist — if this computer is lost, wiped, or reinstalled, the account is gone for good.</p>
                <label class="backup-ack"><input type="checkbox" bind:checked={backupAcknowledged} /> I understand that the account I am replacing has no backup file, and will survive only on this computer.</label>
              {/if}
            {:else if backupCurrentNpub === backupRestoreNpub}
              <p>This backup holds the account already in use here. Restoring it changes nothing.</p>
              <p>Account: <code>{backupRestoreNpub}</code></p>
            {:else}
              <p>No account exists on this computer yet, so nothing is replaced.</p>
              <p>Restoring: <code>{backupRestoreNpub}</code></p>
            {/if}
          {/if}
          {#if backupError}<p class="backup-error">{backupError}</p>{/if}
        </div></div>
        <div class="dialog-actions">{#if backupDialog === 'import-confirm'}<button class="classic-button primary" disabled={backupBusy || (!!backupCurrentNpub && backupCurrentNpub !== backupRestoreNpub && !backupAcknowledged)} onclick={() => void confirmRestore()}>{backupBusy ? 'Replacing…' : 'Replace my account'}</button><button class="classic-button" disabled={backupBusy} onclick={closeBackupDialog}>Cancel</button>{:else}<button class="classic-button primary" disabled={backupBusy} onclick={() => void submitBackup()}>{backupBusy ? 'Working…' : backupDialog === 'export' ? 'Encrypt and save' : 'Continue'}</button><button class="classic-button" disabled={backupBusy} onclick={closeBackupDialog}>Cancel</button>{/if}</div>
      </dialog>
    </div>
  {/if}

  {#if blockConfirmation}
    <div class="modal-backdrop" role="presentation" onclick={() => { if (!blockInProgress) blockConfirmation = null; }}>
      <dialog class="dialog confirm-dialog" open aria-label="Confirm block" onclick={(e) => e.stopPropagation()} onkeydown={(e) => { if (e.key === 'Escape' && !blockInProgress) blockConfirmation = null; }}>
        <header class="titlebar"><div class="title-left"><span class="app-icon">!</span><span>Confirm block</span></div><div class="window-controls"><button disabled={blockInProgress} onclick={() => (blockConfirmation = null)}>×</button></div></header>
        <div class="dialog-body"><div class="confirm-icon">!</div><div><h3>Are you sure?</h3>{#if blockConfirmation.kind === 'file'}<p>Block <strong>{blockConfirmation.label}</strong>?</p><p>Every seeder offering these exact file bytes will be hidden.</p>{:else}<p>Block <strong>{blockConfirmation.label}</strong>?</p><p>Their catalogue entries, public chat messages, and download requests will be ignored.</p>{/if}</div></div>
        <div class="dialog-actions"><button class="classic-button primary" disabled={blockInProgress} onclick={confirmBlock}>{blockInProgress ? 'Blocking…' : 'Block'}</button><button class="classic-button" disabled={blockInProgress} onclick={() => (blockConfirmation = null)}>Cancel</button></div>
      </dialog>
    </div>
  {/if}
</main>
{/if}
