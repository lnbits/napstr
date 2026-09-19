<script lang="ts">
  import { t, locale, message as msg, initializeLocale, type Message } from '@napstr/i18n/svelte';
  import LanguageSelect from '@napstr/i18n/LanguageSelect.svelte';
  import { readStoredPreference } from '@napstr/i18n';
  import { locale as osLocale } from '@tauri-apps/plugin-os';
  import '@napstr/i18n/styles.css';
  import { onMount, tick } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';
  import { invoke } from '@tauri-apps/api/core';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { open } from '@tauri-apps/plugin-dialog';

  let appVersion = '…';
  const SEARCH_PAGE_SIZE = 100;
  const LOCAL_PAGE_SIZE = 100;
  const CHAT_PAGE_SIZE = 100;
  const VISIBLE_SEEDER_LIMIT = 100;

  type View = 'Search' | 'Downloads' | 'Shared' | 'Profile' | 'Settings' | 'Trollbox' | 'Napstrfy';
  type PlayerMode = 'single' | 'folder' | 'all';
  type PlayerOrigin = 'search' | 'downloads' | 'shared' | 'audiobook' | 'direct';
  type WindowResizeDirection = 'East' | 'North' | 'NorthEast' | 'NorthWest' | 'South' | 'SouthEast' | 'SouthWest' | 'West';
  type Result = {
    id: number;
    name: string;
    format: string;
    size: string;
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
    audiobook?: Audiobook;
  };
  type CatalogueUser = { pubkey: string; npub: string; displayName: string };
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
    { label: 'Trollbox', icon: '▣' },
    { label: 'Napstrfy', icon: '▯' }
  ];

  type NativeFile = { fileId: string; filename: string; path: string; folder: string; size: number; format: string; status: string; title: string; artist: string; album: string; mime: string; license: string; description: string; tags: string };
  type NativeTransfer = { id: number; fileId: string; filename: string; size: number; progress: number; status: string; speed: string; destination: string };
  type NativeSettings = { napstrFolder: string; nostrRelays: string; displayName: string; profileAbout: string; profilePicture: string };
  type AudiobookChapter = { position: number; fileId: string; filename: string; title: string; format: string; mime: string; size: number };
  type Audiobook = { audiobookId: string; title: string; author: string; narrator: string; totalSize: number; chapters: AudiobookChapter[]; sources: SourceDetail[]; local: boolean; localFolder: string };
  type Snapshot = { files: NativeFile[]; audiobooks: Audiobook[]; transfers: NativeTransfer[]; settings: NativeSettings; indexedBytes: number; native: boolean };
  type NetworkStatus = { connected: boolean; npub: string; pubkey: string; relayCount: number; torRunning: boolean; torStarting: boolean; torProgress: number; torError: string; error: string };
  type NetworkResult = { fileId: string; filename: string; title: string; artist: string; album: string; format: string; mime: string; size: number; license: string; description: string; tags: string; sources: SourceDetail[] };
  type CatalogueBrowseCursor = { sessionId: string };
  type CatalogueBrowsePage = { results: NetworkResult[]; cursor: CatalogueBrowseCursor | null; totalAvailable: number };
  type PlayerTrack = { fileId: string; name: string; folder: string; artist: string; mime: string };
  type PlaybackStatus = { fileId: string; currentTime: number; duration: number; playing: boolean; ended: boolean; error: string };
  type ReleaseStatus = { version: string; url: string };
  type GitHubRelease = { tag_name?: unknown; html_url?: unknown };
  type TrollboxMessage = { eventId: string; pubkey: string; npub: string; displayName: string; content: string; createdAt: number };
  type IndexProgress = { scanning: boolean; processedFiles: number; indexedFiles: number; message: string };
  type IndexBatch = { files: NativeFile[]; fileCount: number; totalBytes: number };
  type MobileDevice = { endpointId: string; name: string; pairedAt: string; lastSeen: string; streamOnly: boolean };
  type MobileStatus = { running: boolean; online: boolean; endpointId: string; error: string; devices: MobileDevice[] };
  type MobilePairingOffer = { ticket: string; qrSvg: string; expiresAt: number; endpointId: string };
  type BlockConfirmation =
    | { kind: 'file'; fileId: string; label: string }
    | { kind: 'user'; pubkey: string; label: string };

  let activeView: View = 'Search';
  let results: Result[] = [];
  let resultPage = 0;
  let query = '';
  let searchUser: CatalogueUser | null = null;
  let resultUser: CatalogueUser | null = null;
  let matchingUsers: CatalogueUser[] = [];
  let format = 'Audio only';
  let minimumSources = 1;
  let maximumSize = '';
  let searchedQuery = 'All audio';
  let resultsAreNetwork = false;
  let selected: Result | null = null;
  let selectedResultIds = new Set<string>();
  let resultSelectionAnchor: string | null = null;
  let downloadingSelection = false;
  let clearingTransfers = false;
  let removingTransfers = new Set<number>();
  let downloadGeneration = 0;
  let nextOptimisticTransferId = Date.now();
  let downloadLibraryRefreshNeeded = false;
  const pendingDownloadRequests = new Map<Promise<unknown>, string>();
  const downloadAttempts = new Map<string, { cancelled: boolean }>();
  const cancellingFiles = new Set<string>();
  let advanced = false;
  let paused = false;
  let aboutOpen = false;
  let sourceProfile: SourceDetail | null = null;
  let blockConfirmation: BlockConfirmation | null = null;
  let blockInProgress = false;
  let startingDownloads = new Set<string>();
  // Format the clock in the template: a legacy $: declaration here changes
  // dependency tracking for the entire component and freezes helper-based views.
  let clockNow = Date.now();
  let desktopRuntime = false;
  let nativeReady = false;
  let activityMessage: string | Message = msg("Starting Napstr…");
  let napstrFolder = '';
  let nostrRelays = 'wss://relay.damus.io, wss://nos.lol, wss://relay.nostr.com, wss://relay.primal.net, wss://relay.snort.social, wss://nostr.mom, wss://relay.nostr.band';
  let displayName = 'napstr-user';
  let profileAbout = 'Sharing files privately with Napstr. napstr.net';
  let profilePicture = '';
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
  let trollboxError: string | Message = '';
  let trollboxPollPending = false;
  let trollboxRefreshAgain = false;
  let trollboxHasMore = true;
  let mobileStatusValue: MobileStatus | null = null;
  let mobilePairing: MobilePairingOffer | null = null;
  let mobileStreamPairing: MobilePairingOffer | null = null;
  let mobileStreamOnly = false;
  let mobileLoading = false;
  let mobileStatusPending = false;
  let mobileError: string | Message = '';
  let trollboxLog: HTMLDivElement;
  let trackDiscussionFileId = '';
  let trackDiscussionMessages: TrollboxMessage[] = [];
  let trackDiscussionDraft = '';
  let trackDiscussionLoading = false;
  let trackDiscussionSending = false;
  let trackDiscussionError = '';
  let trackDiscussionPollPending = false;
  let trackDiscussionRefreshAgain = false;
  let trackDiscussionSubscribeAgain = false;
  let trackDiscussionHasMore = true;
  let trackDiscussionGeneration = 0;
  let trackDiscussionLog: HTMLDivElement;
  let searchAction: 'search' | 'surprise' | null = null;
  let browseCursor: CatalogueBrowseCursor | null = null;
  let browseLoading = false;
  let browseGeneration = 0;
  let loadedNetworkMatches: NetworkResult[] = [];
  let loadedNetworkAudiobooks: Audiobook[] = [];
  let browseTotalAvailable = 0;
  let rescanPending = false;
  let indexing = false;
  let downloadLibraryPage = 0;
  let sharedLibraryPage = 0;
  let selectedSource = 0;
  let selectedShared: NativeFile | null = null;
  let selectedTagFile: NativeFile | null = null;
  let tagDraft = '';
  let tagSaving = false;
  let libraryFolderView = '*';
  let libraryFolderMenuOpen = false;
  let playerMode: PlayerMode = 'single';
  let playerOrigin: PlayerOrigin = 'direct';
  let activePlayerAudiobook: Audiobook | null = null;
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
  let compactMode = false;
  let compactModePending = false;
  let transferPaneHeight = 119;
  let stopTransferResize = () => {};
  let transfers: Transfer[] = [];
  let localAudiobooks: Audiobook[] = [];
  let audiobookEditorOpen = false;
  let audiobookSaving = false;
  let audiobookTitle = '';
  let audiobookAuthor = '';
  let audiobookNarrator = '';
  type AudiobookDownload = { audiobookId: string; title: string; author: string; narrator: string; destinationFolder: string; chapters: AudiobookChapter[]; sources: SourceDetail[]; nextIndex: number; activeFileId: string; failed: number };
  let audiobookDownloads: AudiobookDownload[] = [];

  let sharedFiles: Array<NativeFile & { name: string; readableSize: string; peers: number }> = [];
  let localFileIds = new Set<string>();

  const readableSize = (bytes: number) => {
    if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
    if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(bytes >= 100 * 1024 ** 2 ? 0 : 1)} MB`;
    if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`;
    return `${bytes} B`;
  };

  function mapFiles(files: NativeFile[]): Result[] {
    return files.map((file, index) => ({
      id: index + 1, name: file.title || file.filename, format: file.format, size: readableSize(file.size), sources: 1,
      speed: 'Local', length: '—', fileId: file.fileId, artist: file.artist, album: file.album, license: file.license, description: file.description, tags: file.tags
    }));
  }

  function mapNetworkFiles(files: NetworkResult[]): Result[] {
    return files.map((file, index) => {
      const local = localFileIds.has(file.fileId);
      return {
        id: index + 1, name: file.title || file.filename, format: file.format, size: readableSize(file.size),
        sources: file.sources.length, speed: local ? 'Local' : 'Tor', length: '—', fileId: file.fileId,
        sourceDetails: file.sources, remote: !local, artist: file.artist, album: file.album,
        license: file.license, description: file.description, tags: file.tags
      };
    });
  }

  function audiobookMatches(book: Audiobook, value: string) {
    if (/^audiobooks?$/i.test(value.trim())) return true;
    const fields = [book.title, book.author, book.narrator, ...book.chapters.flatMap((chapter) => [chapter.title, chapter.filename])]
      .join(' ')
      .toLowerCase();
    return value.trim().toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean).every((token) => fields.includes(token));
  }

  function mergeAudiobooks(trackResults: Result[], remoteBooks: Audiobook[], searchValue: string) {
    const audiobookKeyword = /^audiobooks?$/i.test(searchValue.trim());
    if (format !== 'Audiobooks' && !audiobookKeyword) return trackResults;
    const books = new Map<string, Audiobook>();
    for (const incoming of [...localAudiobooks.filter((book) => audiobookMatches(book, searchValue)), ...remoteBooks]) {
      const existing = books.get(incoming.audiobookId);
      if (!existing) {
        books.set(incoming.audiobookId, { ...incoming, sources: [...incoming.sources] });
        continue;
      }
      const sources = new Map(existing.sources.map((source) => [source.pubkey, source]));
      incoming.sources.forEach((source) => sources.set(source.pubkey, source));
      books.set(incoming.audiobookId, {
        ...(existing.local ? existing : incoming),
        local: existing.local || incoming.local,
        localFolder: existing.localFolder || incoming.localFolder,
        sources: [...sources.values()]
      });
    }
    const grouped = [...books.values()].map((book, index): Result => ({
      id: -(index + 1),
      name: book.title,
      format: 'AUDIOBOOK',
      size: readableSize(book.totalSize),
      sources: Math.max(book.local ? 1 : 0, book.sources.length),
      speed: book.local ? 'Local' : 'Tor',
      length: `${book.chapters.length} chapters`,
      fileId: `audiobook:${book.audiobookId}`,
      artist: book.author,
      album: book.narrator ? `Narrated by ${book.narrator}` : '',
      remote: !book.local,
      sourceDetails: book.sources,
      audiobook: book
    }));
    if (format === 'Audiobooks') {
      return grouped
        .sort((left, right) => right.sources - left.sources || left.name.localeCompare(right.name))
        .map((result, index) => ({ ...result, id: index + 1 }));
    }
    // The dedicated media browse presents each collection once. This is only
    // a display filter: normal title/artist searches retain every underlying
    // audio hash, so one publisher cannot hide ordinary tracks by grouping
    // them into a bogus collection.
    const chapterIds = audiobookKeyword
      ? new Set([...books.values()].flatMap((book) => book.chapters.map((chapter) => chapter.fileId)))
      : null;
    const displayedTracks = chapterIds
      ? trackResults.filter((track) => !chapterIds.has(track.fileId))
      : trackResults;
    return [...grouped, ...displayedTracks]
      .sort((left, right) => right.sources - left.sources || left.name.localeCompare(right.name))
      .map((result, index) => ({ ...result, id: index + 1 }));
  }

  function matchesType(mime: string, fileFormat: string) {
    return mime.startsWith('audio/') && ['MP3', 'FLAC', 'WAV', 'OGG', 'OPUS'].includes(fileFormat.toUpperCase());
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
    );
  }

  function mergeSearchResults(networkMatches: NetworkResult[], localMatches: NativeFile[]) {
    const merged = new Map<string, Result>();
    for (const result of mapNetworkFiles(eligibleNetworkMatches(networkMatches))) {
      merged.set(result.fileId, result);
    }
    if (minimumSources <= 1) {
      for (const local of mapFiles(localMatches.filter((item) =>
        item.size <= maximumBytes() && matchesType(item.mime, item.format)
      ))) {
        const existing = merged.get(local.fileId);
        merged.set(local.fileId, existing
          ? { ...existing, remote: false, speed: 'Local', sources: Math.max(1, existing.sources) }
          : local);
      }
    }
    return [...merged.values()]
      .sort((left, right) => right.sources - left.sources || left.name.localeCompare(right.name))
      .map((result, index) => ({ ...result, id: index + 1 }));
  }

  function mergeNetworkPages(existing: NetworkResult[], incoming: NetworkResult[]) {
    const merged = new Map(existing.map((item) => [item.fileId, item]));
    for (const item of incoming) {
      const previous = merged.get(item.fileId);
      if (!previous) {
        merged.set(item.fileId, item);
        continue;
      }
      const sources = new Map(previous.sources.map((source) => [source.pubkey, source]));
      for (const source of item.sources) sources.set(source.pubkey, source);
      merged.set(item.fileId, { ...previous, ...item, sources: [...sources.values()] });
    }
    return [...merged.values()];
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

  function isActiveTransfer(transfer: Pick<Transfer, 'progress' | 'status'>) {
    // Receiving all bytes is not completion: verification and indexing still
    // need to finish before the download disappears from the native queue.
    return transfer.status !== 'Verified · Complete' && !/^(Failed|Cancelled|Refused|All seeders refused)/.test(transfer.status);
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

  function mergePendingTransfers(items: NativeTransfer[]): Transfer[] {
    const updated = mapTransfers(items);
    const pending = transfers.filter((transfer) => startingDownloads.has(transfer.fileId)
      && !updated.some((item) => item.fileId === transfer.fileId));
    return showAvailableDownloadSlots([...pending, ...updated]);
  }

  function showAvailableDownloadSlots(items: Transfer[]): Transfer[] {
    items = items.map((item) => item.status === 'Starting download'
      ? { ...item, status: 'Queued', speed: 'Queued' } : item);
    let available = Math.max(0, 2 - items.filter((item) => isActiveTransfer(item)
      && item.status !== 'Queued' && item.status !== 'Waiting to restart after reconnect').length);
    // Native rows and optimistic rows are newest first. Reserve free slots for
    // the oldest queued tracks while the native dispatcher starts their requests.
    return items.slice().reverse().map((item) => {
      if (!paused && item.status === 'Queued' && available > 0) {
        available -= 1;
        return { ...item, status: 'Starting download', speed: 'Connecting…' };
      }
      return item;
    }).reverse();
  }

  async function applyTransferUpdate(items: NativeTransfer[]) {
    const generation = downloadGeneration;
    const updated = mergePendingTransfers(items);
    const completed = updated.filter((transfer) => isCompleteTransfer(transfer) && !isLocalFile(transfer.fileId));
    const vanished = transfers.filter((transfer) => !startingDownloads.has(transfer.fileId)
      && isActiveTransfer(transfer) && !updated.some((item) => item.fileId === transfer.fileId));
    transfers = updated;
    if (completed.length || vanished.length) downloadLibraryRefreshNeeded = true;
    if (downloadLibraryRefreshNeeded) {
      if (await refreshLocalLibrary()) downloadLibraryRefreshNeeded = false;
      if (generation !== downloadGeneration) return;
      const latest = completed[0] ?? vanished.find((transfer) => isLocalFile(transfer.fileId));
      if (latest) activityMessage = msg("{p0} downloaded, verified, and ready to play", { p0: latest.name });
    }
  }

  function isLocalFile(fileId: string) {
    return localFileIds.has(fileId);
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

  function audiobookFolderFiles() {
    if (libraryFolderView === '*') return [];
    const prefix = `${libraryFolderView}/`;
    return sharedFiles
      .filter((file) => file.folder === libraryFolderView || file.folder.startsWith(prefix))
      .sort((left, right) => `${left.folder}/${left.filename}`.localeCompare(`${right.folder}/${right.filename}`, undefined, { numeric: true, sensitivity: 'base' }));
  }

  function localPageCount(files: NativeFile[]) {
    return Math.max(1, Math.ceil(files.length / LOCAL_PAGE_SIZE));
  }

  function paginatedTagFiles() {
    const start = downloadLibraryPage * LOCAL_PAGE_SIZE;
    return sharedFiles.slice(start, start + LOCAL_PAGE_SIZE);
  }

  function paginatedSharedFiles() {
    const files = visibleSharedFiles();
    const start = sharedLibraryPage * LOCAL_PAGE_SIZE;
    return files.slice(start, start + LOCAL_PAGE_SIZE);
  }

  function localPageRange(page: number, total: number) {
    if (!total) return '0';
    const start = page * LOCAL_PAGE_SIZE + 1;
    return `${start}–${Math.min(start + LOCAL_PAGE_SIZE - 1, total)}`;
  }

  function changeDownloadLibraryPage(nextPage: number) {
    downloadLibraryPage = Math.max(0, Math.min(nextPage, localPageCount(sharedFiles) - 1));
  }

  function changeSharedLibraryPage(nextPage: number) {
    const files = visibleSharedFiles();
    sharedLibraryPage = Math.max(0, Math.min(nextPage, localPageCount(files) - 1));
    selectedShared = paginatedSharedFiles()[0] ?? null;
  }

  function changeLibraryFolder() {
    sharedLibraryPage = 0;
    selectedShared = visibleSharedFiles()[0] ?? null;
  }

  function selectLibraryFolder(folder: string) {
    libraryFolderView = folder;
    libraryFolderMenuOpen = false;
    changeLibraryFolder();
  }

  function containLibraryFolderMenu(node: HTMLElement) {
    const outside = (event: PointerEvent) => {
      if (libraryFolderMenuOpen && event.target instanceof Node && !node.contains(event.target)) {
        libraryFolderMenuOpen = false;
      }
    };
    const focusLeft = (event: FocusEvent) => {
      if (!(event.relatedTarget instanceof Node) || !node.contains(event.relatedTarget)) {
        libraryFolderMenuOpen = false;
      }
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      libraryFolderMenuOpen = false;
      node.querySelector<HTMLButtonElement>('.folder-picker-toggle')?.focus();
    };
    document.addEventListener('pointerdown', outside);
    node.addEventListener('focusout', focusLeft);
    node.addEventListener('keydown', escape);
    return {
      destroy() {
        document.removeEventListener('pointerdown', outside);
        node.removeEventListener('focusout', focusLeft);
        node.removeEventListener('keydown', escape);
      }
    };
  }

  function resultPageCount() {
    return Math.max(1, Math.ceil(results.length / SEARCH_PAGE_SIZE));
  }

  function paginatedResults() {
    const start = resultPage * SEARCH_PAGE_SIZE;
    return results.slice(start, start + SEARCH_PAGE_SIZE);
  }

  function resultRange() {
    if (!results.length) return '0';
    const start = resultPage * SEARCH_PAGE_SIZE + 1;
    return `${start}–${Math.min(start + SEARCH_PAGE_SIZE - 1, results.length)}`;
  }

  function availableResultTotal() {
    return Math.max(results.length, browseTotalAvailable);
  }

  async function changeResultPage(nextPage: number) {
    if (nextPage >= resultPageCount() && browseCursor && !browseLoading) {
      await loadNextBrowsePage();
    }
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
      activityMessage = msg("Tags saved locally");
      if (networkConnected) {
        try {
          await invoke('publish_catalogue');
          activityMessage = msg("Tags saved and queued for Nostr publication");
        } catch (error) {
          activityMessage = msg("Tags saved locally · Nostr publication will retry later: {p0}", { p0: String(error) });
        }
      }
    } catch (error) {
      activityMessage = msg("Could not save tags: {p0}", { p0: String(error) });
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
    } else if (origin === 'audiobook' && activePlayerAudiobook) {
      queue = activePlayerAudiobook.chapters.flatMap((chapter) => {
        const file = sharedFiles.find((candidate) => candidate.fileId === chapter.fileId);
        return file ? [toPlayerTrack(file)] : [];
      });
    }
    return queue.some((item) => item.fileId === track.fileId) ? queue : [track];
  }

  function queueForTrack(track: PlayerTrack, mode: PlayerMode, origin: PlayerOrigin = playerOrigin) {
    const library = sortedLibraryTracks();
    if (!library.some((item) => item.fileId === track.fileId)) return [track];
    const contextualQueue = contextualPlayerQueue(track, origin);
    if (origin === 'audiobook') return mode === 'single' ? [track] : contextualQueue;
    if (origin !== 'direct') {
      if (mode === 'folder') return contextualQueue.filter((item) => item.folder === track.folder);
      return contextualQueue;
    }
    if (mode === 'all') return library;
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
      const index = sharedFiles.findIndex((item) => item.fileId === track.fileId);
      if (index >= 0) {
        downloadLibraryPage = Math.floor(index / LOCAL_PAGE_SIZE);
        selectTagFile(sharedFiles[index]);
      }
    } else if (playerOrigin === 'shared') {
      const file = sharedFiles.find((item) => item.fileId === track.fileId);
      if (file) {
        const index = visibleSharedFiles().findIndex((item) => item.fileId === track.fileId);
        if (index >= 0) sharedLibraryPage = Math.floor(index / LOCAL_PAGE_SIZE);
        selectedShared = { ...file };
      }
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
      if (!lastPlayerError) activityMessage = msg("Playing {p0}{p1}", { p0: track.name, p1: track.folder ? ` · ${track.folder}` : '' });
    } catch (error) {
      playerPlaying = false;
      playerEnded = true;
      lastPlayerError = String(error);
      activityMessage = msg("Playback failed: {p0}", { p0: lastPlayerError });
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
    activePlayerAudiobook = null;
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
      else activityMessage = msg("Select a local song to play");
      return;
    }
    if (playerEnded) {
      await loadPlayerTrack(playerQueueIndex);
      return;
    }
    try { applyPlaybackStatus(await invoke<PlaybackStatus>('toggle_audio')); }
    catch (error) { activityMessage = msg("Playback failed: {p0}", { p0: String(error) }); }
  }

  async function stopPlayer() {
    try { applyPlaybackStatus(await invoke<PlaybackStatus>('stop_audio')); }
    catch (error) { activityMessage = msg("Could not stop playback: {p0}", { p0: String(error) }); return; }
    // The native output stream is deliberately released on Stop. Treat the
    // track as reloadable so pressing Play opens it again from the beginning.
    playerEnded = true;
    if (currentTrack) activityMessage = msg("Stopped {p0}", { p0: currentTrack.name });
  }

  async function nextPlayerTrack() {
    if (playerQueueIndex + 1 < playerQueue.length) await loadPlayerTrack(playerQueueIndex + 1);
    else stopPlayer();
  }

  async function previousPlayerTrack() {
    if (playerCurrentTime > 3 || playerQueueIndex <= 0) {
      try { applyPlaybackStatus(await invoke<PlaybackStatus>('seek_audio', { seconds: 0 })); }
      catch (error) { activityMessage = msg("Could not rewind playback: {p0}", { p0: String(error) }); }
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
    } catch (error) { activityMessage = msg("Could not seek in this track: {p0}", { p0: String(error) }); }
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
      if (status.error !== lastPlayerError) activityMessage = msg("Playback failed: {p0}", { p0: status.error });
      lastPlayerError = status.error;
      return;
    }
    lastPlayerError = '';
    playerCurrentTime = status.currentTime;
    playerDuration = status.duration;
    playerPlaying = status.playing;
  }

  function syncResultLocality() {
    if (resultUser?.npub === identityNpub) {
      results = mergeSearchResults([], sharedFiles);
    } else if (resultsAreNetwork) {
      results = results.map((result) => {
        if (result.audiobook) {
          const localBook = localAudiobooks.find((book) => book.audiobookId === result.audiobook?.audiobookId);
          const audiobook = localBook ? { ...result.audiobook, local: true, localFolder: localBook.localFolder } : result.audiobook;
          return { ...result, audiobook, remote: !audiobook.local, speed: audiobook.local ? 'Local' : 'Tor' };
        }
        const local = isLocalFile(result.fileId);
        return { ...result, remote: !local, speed: local ? 'Local' : 'Tor' };
      });
    } else if (!query.trim() || searchedQuery === 'local catalogue' || searchedQuery === 'All audio') {
      results = mergeAudiobooks(mapFiles(sharedFiles), [], query.trim());
    } else {
      results = results.filter((result) => isLocalFile(result.fileId));
    }
    resultPage = Math.min(resultPage, resultPageCount() - 1);
    reconcileResultSelection();
  }

  function applySnapshot(snapshot: Snapshot) {
    nativeReady = snapshot.native;
    indexedBytes = snapshot.indexedBytes;
    napstrFolder = snapshot.settings.napstrFolder;
    nostrRelays = snapshot.settings.nostrRelays;
    displayName = snapshot.settings.displayName;
    profileAbout = snapshot.settings.profileAbout;
    profilePicture = snapshot.settings.profilePicture;
    localAudiobooks = snapshot.audiobooks;
    sharedFiles = snapshot.files.map((file) => ({ ...file, name: file.filename, readableSize: readableSize(file.size), peers: 0 }));
    localFileIds = new Set(snapshot.files.map((file) => file.fileId));
    downloadLibraryPage = Math.min(downloadLibraryPage, localPageCount(snapshot.files) - 1);
    sharedLibraryPage = Math.min(sharedLibraryPage, localPageCount(visibleSharedFiles()) - 1);
    if (selectedShared) selectedShared = snapshot.files.find((file) => file.fileId === selectedShared?.fileId) ?? null;
    resultUser = null;
    searchUser = null;
    matchingUsers = [];
    results = mergeAudiobooks(mapFiles(snapshot.files), [], '');
    resultPage = 0;
    resultsAreNetwork = false;
    selectResult(results[0] ?? null);
    searchedQuery = 'local catalogue';
    transfers = mapTransfers(snapshot.transfers);
    activityMessage = snapshot.files.length ? msg("{p0} local files indexed and ready", { p0: snapshot.files.length }) : msg("Choose a Napstr folder to begin");
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
      activityMessage = msg("Could not open the release page: {p0}", { p0: String(error) });
    }
  }

  async function openNapstrfyWebsite(event: MouseEvent) {
    event.preventDefault();
    try {
      await invoke('open_napstrfy_website');
    } catch (error) {
      activityMessage = msg("Could not open the Napstrfy website: {p0}", { p0: String(error) });
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

  function mergeChatMessages(current: TrollboxMessage[], incoming: TrollboxMessage[]) {
    return [...new Map([...current, ...incoming].map((message) => [message.eventId, message])).values()]
      .sort((a, b) => a.createdAt - b.createdAt || a.eventId.localeCompare(b.eventId));
  }

  function openChatLog(node: HTMLDivElement) {
    let alive = true;
    void tick().then(() => { if (alive) node.scrollTop = node.scrollHeight; });
    return { destroy() { alive = false; } };
  }

  function chatScrollRestorer(node: HTMLDivElement | undefined, initial: boolean, older: boolean) {
    if (!node) return () => {};
    const atBottom = node.scrollHeight - node.scrollTop - node.clientHeight < 40;
    if (initial || (!older && atBottom)) return () => { node.scrollTop = node.scrollHeight; };
    // Keep the same visible message in place, including if the user moved while
    // the network request was pending or a loading notice changes height.
    const anchor = [...node.querySelectorAll<HTMLElement>('.trollbox-message')]
      .find((item) => item.getBoundingClientRect().bottom > node.getBoundingClientRect().top);
    const offset = anchor ? anchor.getBoundingClientRect().top - node.getBoundingClientRect().top : 0;
    return () => {
      if (anchor?.isConnected) node.scrollTop += anchor.getBoundingClientRect().top - node.getBoundingClientRect().top - offset;
    };
  }

  async function refreshTrollbox(older = false) {
    if (!nativeReady || !networkConnected || (older && (!trollboxHasMore || !trollboxMessages.length))) return;
    if (trollboxPollPending) {
      if (!older) trollboxRefreshAgain = true;
      return;
    }
    trollboxPollPending = true;
    const initial = trollboxMessages.length === 0;
    trollboxLoading = initial || older;
    const first = trollboxMessages[0];
    try {
      const messages = await invoke<TrollboxMessage[]>('get_trollbox_messages', {
        before: older ? { createdAt: first.createdAt, eventId: first.eventId } : null
      });
      const restoreScroll = chatScrollRestorer(trollboxLog, initial, older);
      trollboxMessages = mergeChatMessages(trollboxMessages, messages);
      if (older) trollboxHasMore = messages.length === CHAT_PAGE_SIZE;
      trollboxError = '';
      trollboxLoading = false;
      await tick();
      restoreScroll();
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
    if (view === 'Napstrfy') void openMobileConnect();
  }

  async function openMobileConnect() {
    mobileStreamOnly = false;
    await refreshMobileStatus();
    if (!mobilePairing) await createMobilePairing();
    if (!mobileStreamPairing) await createMobilePairing(true);
  }

  function navigatePairingTabs(event: KeyboardEvent) {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    mobileStreamOnly = event.key === 'Home' ? false : event.key === 'End' ? true : !mobileStreamOnly;
    document.getElementById(`pairing-tab-${mobileStreamOnly ? 'stream' : 'full'}`)?.focus();
  }

  async function refreshMobileStatus() {
    if (!nativeReady || mobileStatusPending) return;
    mobileStatusPending = true;
    try {
      mobileStatusValue = await invoke<MobileStatus>('mobile_status');
      mobileError = mobileStatusValue.error;
    } catch (error) {
      mobileError = String(error);
    } finally {
      mobileStatusPending = false;
    }
  }

  async function createMobilePairing(streamOnly = false) {
    if (!nativeReady || mobileLoading) return;
    mobileLoading = true;
    mobileError = '';
    try {
      const offer = await invoke<MobilePairingOffer>('create_mobile_pairing', { streamOnly });
      if (streamOnly) mobileStreamPairing = offer;
      else mobilePairing = offer;
      await refreshMobileStatus();
    } catch (error) {
      mobileError = String(error);
    } finally {
      mobileLoading = false;
    }
  }

  async function revokeMobileDevice(device: MobileDevice) {
    if (!window.confirm($t("Remove {p0}? It will need a new QR code before it can connect again.", { p0: device.name }))) return;
    try {
      await invoke('revoke_mobile_device', { endpointId: device.endpointId });
      await refreshMobileStatus();
    } catch (error) {
      mobileError = String(error);
    }
  }

  function mobileLastSeen(value: string) {
    const time = Date.parse(value);
    if (!Number.isFinite(time)) return $t("Never");
    const elapsed = Math.max(0, Date.now() - time);
    if (elapsed < 90_000) return $t("Just now");
    if (elapsed < 3_600_000) return $t("{p0} min ago", { p0: Math.floor(elapsed / 60_000) });
    if (elapsed < 86_400_000) return $t("{p0} hr ago", { p0: Math.floor(elapsed / 3_600_000) });
    return new Date(time).toLocaleDateString($locale);
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

  function selectResult(item: Result | null, forceSubscribe = false, preserveSelection = false) {
    if (!preserveSelection) {
      selectedResultIds = new Set(item ? [item.fileId] : []);
      resultSelectionAnchor = item?.fileId ?? null;
    }
    const changed = selected?.fileId !== item?.fileId;
    selected = item;
    if (changed || !preserveSelection) selectedSource = 0;
    if (!item || item.audiobook) {
      trackDiscussionGeneration += 1;
      trackDiscussionFileId = '';
      trackDiscussionMessages = [];
      trackDiscussionDraft = '';
      trackDiscussionError = '';
    } else if (changed || forceSubscribe) {
      void refreshTrackDiscussion(item.fileId, true);
    }
  }

  function selectResultRange(item: Result, event: MouseEvent | KeyboardEvent) {
    const anchor = results.findIndex((result) => result.fileId === resultSelectionAnchor);
    const end = results.findIndex((result) => result.fileId === item.fileId);
    if (event.shiftKey && anchor >= 0 && end >= 0) {
      selectedResultIds = new Set(results.slice(Math.min(anchor, end), Math.max(anchor, end) + 1).map((result) => result.fileId));
      selectResult(item, false, true);
    } else {
      selectResult(item);
    }
  }

  function reconcileResultSelection(forceSubscribe = false) {
    const remaining = results.filter((item) => selectedResultIds.has(item.fileId));
    const next = remaining.find((item) => item.fileId === selected?.fileId) ?? remaining[0] ?? paginatedResults()[0] ?? null;
    selectedResultIds = new Set(remaining.length ? remaining.map((item) => item.fileId) : next ? [next.fileId] : []);
    if (!results.some((item) => item.fileId === resultSelectionAnchor)) resultSelectionAnchor = next?.fileId ?? null;
    selectResult(next, forceSubscribe, true);
  }

  function selectedResults() {
    return results.filter((item) => selectedResultIds.has(item.fileId));
  }

  function canDownloadResult(item: Result) {
    if (item.audiobook) {
      return !item.audiobook.chapters.every((chapter) => isLocalFile(chapter.fileId))
        && item.audiobook.sources.length > 0
        && !audiobookDownloads.some((download) => download.audiobookId === item.audiobook?.audiobookId);
    }
    return !isLocalFile(item.fileId) && Boolean(item.sourceDetails?.length)
      && !startingDownloads.has(item.fileId)
      && !transfers.some((transfer) => transfer.fileId === item.fileId && isActiveTransfer(transfer));
  }

  async function downloadSelectedResults() {
    if (!nativeReady || downloadingSelection || clearingTransfers) return;
    const generation = downloadGeneration;
    const targets = selectedResults();
    downloadingSelection = true;
    let requested = 0;
    let skipped = 0;
    let failed = 0;
    try {
      // Submit the entire selection immediately. The native queue owns the
      // two-track limit; a slow request or UI refresh must not block enqueueing.
      await Promise.all(targets.map(async (target) => {
        if (generation !== downloadGeneration) return;
        if (!canDownloadResult(target)) { skipped += 1; return; }
        try {
          if (await startDownload(target)) requested += 1;
          else failed += 1;
        } catch { failed += 1; }
      }));
      if (generation === downloadGeneration) activityMessage = msg("Downloads requested: {requested} · Skipped: {skipped} · Failed: {failed}", { requested, skipped, failed });
    } finally {
      downloadingSelection = false;
    }
  }

  async function refreshTrackDiscussion(fileId = selected?.fileId ?? '', subscribe = false, older = false) {
    if (!fileId || !nativeReady || !networkConnected) return;
    if (trackDiscussionFileId !== fileId) {
      trackDiscussionGeneration += 1;
      trackDiscussionFileId = fileId;
      trackDiscussionMessages = [];
      trackDiscussionDraft = '';
      trackDiscussionError = '';
      trackDiscussionHasMore = true;
      trackDiscussionPollPending = false;
      trackDiscussionRefreshAgain = false;
      trackDiscussionSubscribeAgain = false;
      subscribe = true;
    }
    if (older && (!trackDiscussionHasMore || !trackDiscussionMessages.length)) return;
    if (trackDiscussionPollPending) {
      if (!older) trackDiscussionRefreshAgain = true;
      if (subscribe) trackDiscussionSubscribeAgain = true;
      return;
    }
    const generation = trackDiscussionGeneration;
    trackDiscussionPollPending = true;
    const initial = trackDiscussionMessages.length === 0;
    trackDiscussionLoading = initial || older;
    const first = trackDiscussionMessages[0];
    try {
      const messages = await invoke<TrollboxMessage[]>('get_track_discussion_messages', {
        fileId, subscribe, before: older ? { createdAt: first.createdAt, eventId: first.eventId } : null
      });
      if (generation !== trackDiscussionGeneration || selected?.fileId !== fileId) return;
      const restoreScroll = chatScrollRestorer(trackDiscussionLog, initial, older);
      trackDiscussionMessages = mergeChatMessages(trackDiscussionMessages, messages);
      if (older) trackDiscussionHasMore = messages.length === CHAT_PAGE_SIZE;
      trackDiscussionError = '';
      trackDiscussionLoading = false;
      await tick();
      if (generation === trackDiscussionGeneration) restoreScroll();
    } catch (error) {
      if (generation === trackDiscussionGeneration) trackDiscussionError = String(error);
    } finally {
      if (generation === trackDiscussionGeneration) {
        trackDiscussionLoading = false;
        trackDiscussionPollPending = false;
        if (trackDiscussionRefreshAgain) {
          const subscribeAgain = trackDiscussionSubscribeAgain;
          trackDiscussionRefreshAgain = false;
          trackDiscussionSubscribeAgain = false;
          void refreshTrackDiscussion(fileId, subscribeAgain);
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
      localAudiobooks = snapshot.audiobooks;
      const nextFiles = snapshot.files.map((file) => ({ ...file, name: file.filename, readableSize: readableSize(file.size), peers: 0 }));
      const removedCurrentTrack = currentTrack && !nextFiles.some((file) => file.fileId === currentTrack?.fileId);
      sharedFiles = nextFiles;
      localFileIds = new Set(nextFiles.map((file) => file.fileId));
      downloadLibraryPage = Math.min(downloadLibraryPage, localPageCount(nextFiles) - 1);
      sharedLibraryPage = Math.min(sharedLibraryPage, localPageCount(visibleSharedFiles()) - 1);
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
        activityMessage = msg("Stopped playback because the file was removed from the Napstr folder");
      } else if (currentTrack) {
        playerQueue = queueForTrack(currentTrack, playerMode);
        playerQueueIndex = playerQueue.findIndex((item) => item.fileId === currentTrack?.fileId);
      }
      syncResultLocality();
      return true;
    } catch { return false; /* the next folder-watch or transfer poll will retry */ }
  }

  function mergeIndexBatch(batch: IndexBatch) {
    const merged = new Map(sharedFiles.map((file) => [file.fileId, file]));
    for (const file of batch.files) {
      merged.set(file.fileId, {
        ...file,
        name: file.filename,
        readableSize: readableSize(file.size),
        peers: merged.get(file.fileId)?.peers ?? 0
      });
    }
    sharedFiles = [...merged.values()].sort((left, right) => left.filename.localeCompare(right.filename));
    localFileIds = new Set(sharedFiles.map((file) => file.fileId));
    indexedBytes = sharedFiles.reduce((total, file) => total + file.size, 0);
    if (selectedShared) selectedShared = merged.get(selectedShared.fileId) ?? selectedShared;
    if (selectedTagFile) selectedTagFile = merged.get(selectedTagFile.fileId) ?? selectedTagFile;
    if (currentTrack) {
      playerQueue = queueForTrack(currentTrack, playerMode);
      playerQueueIndex = playerQueue.findIndex((item) => item.fileId === currentTrack?.fileId);
    }
    syncResultLocality();
  }

  async function connectNetwork() {
    if (!nativeReady || networkConnectPending) return;
    networkConnectPending = true;
    activityMessage = msg("Connecting to Nostr relays and opening encrypted inbox…");
    try {
      const status = await invoke<NetworkStatus>('start_network');
      applyNetworkStatus(status);
      activityMessage = msg("Nostr connected · loading the most available audio from {p0} relays…", { p0: status.relayCount });
      await search();
      if (status.torError) activityMessage = msg("Tor failed: {p0} · click the connection panel to retry", { p0: status.torError });
    } catch (error) {
      networkConnected = false;
      networkError = String(error);
      activityMessage = msg("Network unavailable: {p0}", { p0: String(error) });
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
    activityMessage = msg("Computer resumed · reconnecting Nostr, Tor, and audio…");
    try {
      await invoke('recover_after_sleep');
    } catch (error) {
      activityMessage = msg("Resume recovery failed: {p0} · click the connection panel to retry", { p0: String(error) });
    }
  }

  function torStatusLabel() {
    if (torRunning) return 'Tor connected';
    if (torError) return 'Tor failed';
    if (torStarting && torProgress > 0) return $t("Tor connecting {p0}%", { p0: torProgress });
    return nativeReady ? 'Tor connecting' : 'Tor unavailable';
  }

  async function browseUser(user: CatalogueUser) {
    activeView = 'Search';
    sourceProfile = null;
    query = user.displayName;
    searchUser = user;
    format = 'Audio only';
    minimumSources = 1;
    maximumSize = '';
    await search(true);
  }

  function ownCatalogueUser(): CatalogueUser {
    return { pubkey: identityNpub, npub: identityNpub, displayName };
  }

  function knownUsersNamed(value: string): CatalogueUser[] {
    const name = value.trim().toLocaleLowerCase();
    const users: CatalogueUser[] = [
      ...results.flatMap((item) => item.sourceDetails ?? []),
      ...trollboxMessages, ...trackDiscussionMessages, ownCatalogueUser()
    ];
    return users.filter((user) => user.npub && user.displayName.trim().toLocaleLowerCase() === name);
  }

  async function search(replacePending = false) {
    if (searchAction && !replacePending) return;
    const generation = ++browseGeneration;
    browseCursor = null;
    browseLoading = false;
    loadedNetworkMatches = [];
    loadedNetworkAudiobooks = [];
    browseTotalAvailable = 0;
    searchAction = 'search';
    resultUser = null;
    matchingUsers = [];
    try {
      const trimmedQuery = query.trim();
      let user = searchUser && [searchUser.displayName, searchUser.npub, searchUser.pubkey].some((name) => name.trim() === trimmedQuery) ? searchUser : null;
      searchUser = user;
      if (!user && trimmedQuery) {
        const known = knownUsersNamed(trimmedQuery);
        try {
          const users = networkConnected ? await invoke<CatalogueUser[]>('resolve_catalogue_user', { query: trimmedQuery }) : [];
          if (generation !== browseGeneration) return;
          matchingUsers = [...new Map([...known, ...users].map((user) => [user.npub, user])).values()];
        } catch { if (generation === browseGeneration) matchingUsers = [...new Map(known.map((user) => [user.npub, user])).values()]; }
        if (generation !== browseGeneration) return;
        if (matchingUsers.length === 1) user = matchingUsers[0];
      }
      if (user) {
        searchUser = user;
        resultUser = user;
        matchingUsers = [];
        searchedQuery = user.displayName;
        format = 'Audio only';
        results = [];
        resultsAreNetwork = true;
        resultPage = 0;
        selectResult(null);
        try {
          if (user.npub === identityNpub) {
            results = mergeSearchResults([], sharedFiles);
          } else {
            const page = await invoke<CatalogueBrowsePage>('network_browse_user', { pubkey: user.pubkey, cursor: null });
            if (generation !== browseGeneration) return;
            loadedNetworkMatches = page.results;
            browseCursor = page.cursor;
            browseTotalAvailable = page.totalAvailable;
            results = mergeSearchResults(loadedNetworkMatches, []);
          }
          activityMessage = msg("{p0} tracks loaded from {p1}", { p0: results.length, p1: user.displayName });
          selectResult(results[0] ?? null, true);
          if (browseCursor) void loadNextBrowsePage();
        } catch (error) {
          if (generation === browseGeneration) activityMessage = msg("Could not browse {p0}: {p1}", { p0: user.displayName, p1: String(error) });
        }
        return;
      }
      searchedQuery = trimmedQuery || (format === 'Audiobooks' ? 'All audiobooks' : 'All audio');
      if (networkConnected) {
        if (format === 'Audiobooks') {
          try {
            loadedNetworkAudiobooks = await invoke<Audiobook[]>('network_search_audiobooks', { query: trimmedQuery });
            if (generation !== browseGeneration) return;
            results = mergeAudiobooks([], loadedNetworkAudiobooks, trimmedQuery);
            resultsAreNetwork = true;
            activityMessage = msg("{p0} audiobook collections found", { p0: results.length });
          } catch (error) {
            if (generation !== browseGeneration) return;
            loadedNetworkAudiobooks = [];
            results = mergeAudiobooks([], [], trimmedQuery);
            resultsAreNetwork = false;
            activityMessage = msg("Global audiobook search failed: {p0} · showing {p1} local collections", { p0: String(error), p1: results.length });
          }
        } else {
        const includeAudiobooks = /^audiobooks?$/i.test(trimmedQuery);
        const audiobookRequest = includeAudiobooks
          ? invoke<Audiobook[]>('network_search_audiobooks', { query: trimmedQuery })
              .then((books) => books, () => [] as Audiobook[])
          : Promise.resolve([] as Audiobook[]);
        const [networkOutcome, localOutcome] = await Promise.allSettled([
          trimmedQuery
            ? invoke<NetworkResult[]>('network_search', { query: trimmedQuery })
            : invoke<CatalogueBrowsePage>('network_browse', { cursor: null, limit: 500, cacheLimit: 10000 }),
          invoke<NativeFile[]>('search_catalog', { query: trimmedQuery })
        ]);
        if (generation !== browseGeneration) return;
        const networkMatches = networkOutcome.status === 'fulfilled'
          ? trimmedQuery
            ? networkOutcome.value as NetworkResult[]
            : (networkOutcome.value as CatalogueBrowsePage).results
          : [];
        const localMatches = localOutcome.status === 'fulfilled' ? localOutcome.value : [];
        loadedNetworkMatches = networkMatches;
        loadedNetworkAudiobooks = [];
        browseCursor = format !== 'Audiobooks' && networkOutcome.status === 'fulfilled' && !trimmedQuery
          ? (networkOutcome.value as CatalogueBrowsePage).cursor
          : null;
        browseTotalAvailable = format !== 'Audiobooks' && networkOutcome.status === 'fulfilled' && !trimmedQuery
          ? (networkOutcome.value as CatalogueBrowsePage).totalAvailable
          : 0;
        results = mergeAudiobooks(mergeSearchResults(networkMatches, localMatches), loadedNetworkAudiobooks, trimmedQuery);
        resultsAreNetwork = networkOutcome.status === 'fulfilled';
        if (networkOutcome.status === 'rejected') {
          activityMessage = localOutcome.status === 'fulfilled'
            ? msg("Global search failed: {p0} · showing {p1} local matches", { p0: String(networkOutcome.reason), p1: results.length })
            : msg("Search failed: {p0}", { p0: String(networkOutcome.reason) });
        } else {
          activityMessage = format === 'Audiobooks'
            ? msg("{p0} audiobook collections found", { p0: results.length })
            : !trimmedQuery
            ? msg("{p0} loaded of {p1} currently available file IDs, ranked by active seeders", { p0: results.length, p1: availableResultTotal() })
            : msg("{p0} available file IDs, ranked by active seeders", { p0: results.length });
        }
        // Audiobook manifests are additive. Let ordinary track results render
        // as soon as they are ready instead of making every search wait for a
        // second relay query and manifest validation pass.
        if (includeAudiobooks) void audiobookRequest.then((books) => {
          if (generation !== browseGeneration) return;
          loadedNetworkAudiobooks = books;
          results = mergeAudiobooks(
            mergeSearchResults(loadedNetworkMatches, localMatches),
            loadedNetworkAudiobooks,
            trimmedQuery
          );
          resultPage = 0;
          reconcileResultSelection(true);
          activityMessage = format === 'Audiobooks'
            ? msg("{p0} audiobook collections found", { p0: results.length })
            : !trimmedQuery
            ? msg("{p0} loaded of {p1} currently available file IDs, ranked by active seeders", { p0: results.length, p1: availableResultTotal() })
            : msg("{p0} available file IDs, ranked by active seeders", { p0: results.length });
        });
        }
      } else if (nativeReady) {
        try {
          const matches = await invoke<NativeFile[]>('search_catalog', { query: query.trim() });
          if (generation !== browseGeneration) return;
          results = mergeAudiobooks(mapFiles(matches.filter((item) => minimumSources <= 1 && item.size <= maximumBytes() && matchesType(item.mime, item.format))), [], query.trim());
          resultsAreNetwork = false;
          activityMessage = msg("{p0} local matches found", { p0: results.length });
        } catch (error) { if (generation === browseGeneration) activityMessage = msg("Search failed: {p0}", { p0: String(error) }); }
      }
      resultPage = 0;
      selectResult(results[0] ?? null, true);
      if (generation === browseGeneration && !query.trim() && browseCursor) {
        void loadNextBrowsePage();
      }
    } finally {
      if (generation === browseGeneration) searchAction = null;
    }
  }

  async function loadNextBrowsePage() {
    const cursor = browseCursor;
    const user = resultUser;
    if (!cursor || browseLoading || (!user && query.trim())) return;
    const generation = browseGeneration;
    browseLoading = true;
    activityMessage = msg("{p0} available file IDs loaded · fetching the next relay page…", { p0: results.length });
    try {
      const page = user
        ? await invoke<CatalogueBrowsePage>('network_browse_user', { pubkey: user.pubkey, cursor })
        : await invoke<CatalogueBrowsePage>('network_browse', { cursor, limit: 500, cacheLimit: 10000 });
      if (generation !== browseGeneration || (!user && query.trim())) return;
      loadedNetworkMatches = mergeNetworkPages(loadedNetworkMatches, page.results);
      browseCursor = page.cursor;
      browseTotalAvailable = page.totalAvailable;
      results = user ? mergeSearchResults(loadedNetworkMatches, []) : mergeAudiobooks(mergeSearchResults(loadedNetworkMatches, sharedFiles), loadedNetworkAudiobooks, '');
      resultsAreNetwork = true;
      reconcileResultSelection();
      activityMessage = user
        ? msg("{p0} loaded of {p1} tracks shared by {p2}", { p0: results.length, p1: availableResultTotal(), p2: user.displayName })
        : msg("{p0} loaded of {p1} currently available file IDs, ranked by active seeders", { p0: results.length, p1: availableResultTotal() });
    } catch (error) {
      if (generation === browseGeneration) activityMessage = msg("Could not load the next catalogue page: {p0}", { p0: String(error) });
    } finally {
      if (generation === browseGeneration) browseLoading = false;
    }
  }

  async function surpriseMe() {
    if (searchAction) return;
    if (!networkConnected) {
      activityMessage = msg("Connect to Nostr before asking for a surprise");
      return;
    }
    const generation = ++browseGeneration;
    browseCursor = null;
    browseLoading = false;
    loadedNetworkMatches = [];
    loadedNetworkAudiobooks = [];
    browseTotalAvailable = 0;
    searchAction = 'surprise';
    searchUser = null;
    resultUser = null;
    matchingUsers = [];
    searchedQuery = 'Surprise me';
    query = '';
    format = 'Audio only';
    minimumSources = 1;
    maximumSize = '';
    activityMessage = msg("Finding 50 random downloadable tracks…");
    try {
      let cursor: CatalogueBrowseCursor | null = null;
      let downloadable: NetworkResult[] = [];
      do {
        const page: CatalogueBrowsePage = await invoke('network_browse', {
          cursor, limit: 100, cacheLimit: 50000, unownedOnly: true
        });
        if (generation !== browseGeneration) return;
        downloadable = eligibleNetworkMatches(mergeNetworkPages(downloadable, page.results))
          .filter((item) => !isLocalFile(item.fileId) && item.sources.length > 0);
        cursor = page.cursor;
      } while (downloadable.length < 50 && cursor);
      results = mapNetworkFiles(shuffled(downloadable).slice(0, 50));
      resultsAreNetwork = true;
      resultPage = 0;
      selectResult(results[0] ?? null, true);
      activityMessage = results.length
        ? msg("Random downloadable tracks found: {p0}", { p0: results.length })
        : msg("No downloadable tracks are currently available");
    } catch (error) {
      if (generation === browseGeneration) activityMessage = msg("Surprise search failed: {p0}", { p0: String(error) });
    } finally {
      if (generation === browseGeneration) searchAction = null;
    }
  }

  async function requestNetworkDownload(args: Record<string, unknown>) {
    if (clearingTransfers || cancellingFiles.has(String(args.fileId))) throw new Error('Download is being cancelled');
    const request = invoke('request_network_download', args);
    pendingDownloadRequests.set(request, String(args.fileId));
    try { return await request; }
    finally { pendingDownloadRequests.delete(request); }
  }

  async function startDownload(target: Result | null = selected): Promise<boolean> {
    if (!target || clearingTransfers || cancellingFiles.has(target.fileId)) return false;
    const generation = downloadGeneration;
    if (target.audiobook) {
      await startAudiobookDownload(target.audiobook);
      return true;
    }
    if (nativeReady && isLocalFile(target.fileId)) {
      await playAudio(target.fileId, target.name, playerMode, 'search');
      return false;
    }
    const activeTransfer = transfers.find((item) => item.fileId === target.fileId && isActiveTransfer(item));
    if (activeTransfer || startingDownloads.has(target.fileId)) {
      activityMessage = msg("{p0} is already downloading", { p0: target.name });
      return false;
    }
    if (nativeReady) {
      const sources = target.sourceDetails ?? [];
      if (!sources.length) { activityMessage = msg("No seeder is available for this file"); return false; }
      startingDownloads = new Set(startingDownloads).add(target.fileId);
      const attempt = { cancelled: false };
      downloadAttempts.set(target.fileId, attempt);
      transfers = showAvailableDownloadSlots([{
        id: ++nextOptimisticTransferId, fileId: target.fileId, name: target.name, size: target.size,
        speed: 'Queued', progress: 0, status: 'Queued', destination: ''
      }, ...transfers]);
      const candidateCount = Math.min(sources.length, 3);
      activityMessage = msg("Finding the fastest Tor connection. Seeders: {p0}…", { p0: candidateCount });
      try {
        await requestNetworkDownload({ fileId: target.fileId, sourcePubkeys: sources.map((source) => source.pubkey) });
        if (generation !== downloadGeneration || attempt.cancelled) return false;
        const updated = await invoke<NativeTransfer[]>('get_transfers');
        if (generation !== downloadGeneration || attempt.cancelled) return false;
        await applyTransferUpdate(updated);
        activityMessage = msg("Seeder race started · the fastest responsive source will stream the file");
        return true;
      } catch (error) {
        if (generation !== downloadGeneration || attempt.cancelled) return false;
        try {
          const updated = await invoke<NativeTransfer[]>('get_transfers');
          if (generation !== downloadGeneration || attempt.cancelled) return false;
          await applyTransferUpdate(updated);
        } catch {
          if (generation !== downloadGeneration || attempt.cancelled) return false;
          transfers = transfers.filter((item) => item.fileId !== target.fileId);
        }
        activityMessage = msg("Request failed: {p0}", { p0: String(error) });
      } finally {
        downloadAttempts.delete(target.fileId);
        const nextStarting = new Set(startingDownloads);
        nextStarting.delete(target.fileId);
        startingDownloads = nextStarting;
      }
    }
    return false;
  }

  async function playSelectedAudio() {
    if (!selected) return;
    if (selected.audiobook) {
      await playAudiobook(selected.audiobook);
      return;
    }
    if (!isLocalFile(selected.fileId)) return;
    await playAudio(selected.fileId, selected.name, playerMode, 'search');
  }

  function currentFolderAudiobook() {
    if (libraryFolderView === '*') return null;
    return localAudiobooks.find((book) => book.localFolder === libraryFolderView) ?? null;
  }

  function openAudiobookEditor() {
    if (libraryFolderView === '*' || audiobookFolderFiles().length < 1) return;
    const existing = currentFolderAudiobook();
    const files = audiobookFolderFiles();
    const folderTitle = folderName(libraryFolderView).split('/').at(-1) ?? 'Audiobook';
    audiobookTitle = existing?.title || files.find((file) => file.album)?.album || folderTitle.replace(/[_-]+/g, ' ');
    audiobookAuthor = existing?.author || files.find((file) => file.artist)?.artist || '';
    audiobookNarrator = existing?.narrator || '';
    audiobookEditorOpen = true;
  }

  async function saveAudiobookGroup() {
    if (audiobookSaving || libraryFolderView === '*') return;
    audiobookSaving = true;
    try {
      localAudiobooks = await invoke<Audiobook[]>('save_audiobook', {
        folder: libraryFolderView,
        title: audiobookTitle,
        author: audiobookAuthor,
        narrator: audiobookNarrator
      });
      audiobookEditorOpen = false;
      activityMessage = msg("{p0} grouped and queued for Nostr publication", { p0: audiobookTitle.trim() });
      syncResultLocality();
    } catch (error) {
      activityMessage = msg("Could not group audiobook: {p0}", { p0: String(error) });
    } finally {
      audiobookSaving = false;
    }
  }

  async function ungroupAudiobook() {
    const existing = currentFolderAudiobook();
    if (!existing || audiobookSaving) return;
    audiobookSaving = true;
    try {
      localAudiobooks = await invoke<Audiobook[]>('remove_audiobook', { folder: existing.localFolder });
      audiobookEditorOpen = false;
      activityMessage = msg("{p0} is now published as individual tracks only", { p0: existing.title });
      syncResultLocality();
    } catch (error) {
      activityMessage = msg("Could not remove audiobook grouping: {p0}", { p0: String(error) });
    } finally {
      audiobookSaving = false;
    }
  }

  async function playAudiobook(book: Audiobook) {
    const firstReadyChapter = book.chapters.find((chapter) => isLocalFile(chapter.fileId));
    if (!firstReadyChapter) {
      activityMessage = msg("Download at least the first chapter before playing this audiobook");
      return;
    }
    await playAudiobookChapter(book, firstReadyChapter.fileId);
  }

  async function playAudiobookChapter(book: Audiobook, fileId: string) {
    const queue = book.chapters.flatMap((chapter) => {
      const file = sharedFiles.find((candidate) => candidate.fileId === chapter.fileId);
      return file ? [toPlayerTrack(file)] : [];
    });
    const index = queue.findIndex((chapter) => chapter.fileId === fileId);
    if (index < 0) {
      activityMessage = msg("That chapter has not finished downloading yet");
      return;
    }
    activePlayerAudiobook = book;
    playerOrigin = 'audiobook';
    playerMode = 'all';
    playerQueue = queue;
    await loadPlayerTrack(index);
  }

  function audiobookChapterStatus(book: Audiobook, chapter: AudiobookChapter) {
    if (isLocalFile(chapter.fileId)) return 'Ready';
    const download = audiobookDownloads.find((item) => item.audiobookId === book.audiobookId);
    return download?.activeFileId === chapter.fileId ? 'Downloading' : 'Waiting';
  }

  async function requestNextAudiobookChapter(audiobookId: string) {
    if (clearingTransfers) return;
    const queue = audiobookDownloads.find((item) => item.audiobookId === audiobookId);
    if (!queue || queue.activeFileId) return;
    while (queue.nextIndex < queue.chapters.length && isLocalFile(queue.chapters[queue.nextIndex].fileId)) queue.nextIndex += 1;
    if (queue.nextIndex >= queue.chapters.length) {
      const missing = queue.chapters.filter((chapter) => !isLocalFile(chapter.fileId)).length;
      if (missing) {
        audiobookDownloads = audiobookDownloads.filter((item) => item.audiobookId !== audiobookId);
        activityMessage = msg("{p0} · Missing chapters: {p1}. Select the book to retry.", { p0: queue.title, p1: missing });
        return;
      }
      if (queue.destinationFolder) {
        try {
          localAudiobooks = await invoke<Audiobook[]>('save_audiobook', {
            folder: `Audiobooks/${queue.destinationFolder}`,
            title: queue.title,
            author: queue.author,
            narrator: queue.narrator
          });
          syncResultLocality();
        } catch { /* downloaded chapters remain valid and can be grouped manually */ }
      }
      audiobookDownloads = audiobookDownloads.filter((item) => item.audiobookId !== audiobookId);
      activityMessage = msg("{p0} downloaded and ready to play", { p0: queue.title });
      return;
    }
    const chapter = queue.chapters[queue.nextIndex];
    queue.activeFileId = chapter.fileId;
    audiobookDownloads = [...audiobookDownloads];
    startingDownloads = new Set(startingDownloads).add(chapter.fileId);
    try {
      await requestNetworkDownload({
        fileId: chapter.fileId,
        sourcePubkeys: queue.sources.map((source) => source.pubkey),
        destinationFolder: queue.destinationFolder
      });
      if (!audiobookDownloads.includes(queue)) return;
      await applyTransferUpdate(await invoke<NativeTransfer[]>('get_transfers'));
      activityMessage = msg("Downloading {p0} · chapter {p1} of {p2}", { p0: queue.title, p1: queue.nextIndex + 1, p2: queue.chapters.length });
    } catch (error) {
      if (!audiobookDownloads.includes(queue)) return;
      queue.failed += 1;
      queue.nextIndex += 1;
      queue.activeFileId = '';
      audiobookDownloads = [...audiobookDownloads];
      activityMessage = msg("Chapter {p0} could not start: {p1} · continuing with the book", { p0: queue.nextIndex, p1: String(error) });
      void requestNextAudiobookChapter(audiobookId);
    } finally {
      const nextStarting = new Set(startingDownloads);
      nextStarting.delete(chapter.fileId);
      startingDownloads = nextStarting;
    }
  }

  async function advanceAudiobookDownloads() {
    for (const queue of [...audiobookDownloads]) {
      if (queue.activeFileId && isLocalFile(queue.activeFileId)) {
        queue.nextIndex += 1;
        queue.activeFileId = '';
      } else if (queue.activeFileId) {
        const transfer = transfers.find((item) => item.fileId === queue.activeFileId);
        if (transfer && isFinishedTransfer(transfer) && !isCompleteTransfer(transfer)) {
          queue.failed += 1;
          queue.nextIndex += 1;
          queue.activeFileId = '';
        }
      }
      audiobookDownloads = [...audiobookDownloads];
      if (!queue.activeFileId) await requestNextAudiobookChapter(queue.audiobookId);
    }
  }

  async function startAudiobookDownload(book: Audiobook) {
    if (clearingTransfers) return;
    if (book.chapters.every((chapter) => isLocalFile(chapter.fileId))) {
      await playAudiobook(book);
      return;
    }
    if (!book.sources.length) {
      activityMessage = msg("No complete audiobook seeder is currently available");
      return;
    }
    if (audiobookDownloads.some((item) => item.audiobookId === book.audiobookId)) {
      activityMessage = msg("{p0} is already in the download queue", { p0: book.title });
      return;
    }
    audiobookDownloads = [...audiobookDownloads, {
      audiobookId: book.audiobookId,
      title: book.title,
      author: book.author,
      narrator: book.narrator,
      destinationFolder: `${book.title.replace(/[\x00-\x1f/\\:*?"<>|]/g, '_').replace(/^[.\s]+|[.\s]+$/g, '').slice(0, 86) || 'Audiobook'} [${book.audiobookId.slice(0, 8)}]`,
      chapters: book.chapters,
      sources: book.sources,
      nextIndex: 0,
      activeFileId: '',
      failed: 0
    }];
    await requestNextAudiobookChapter(book.audiobookId);
  }

  function selectedAudiobookComplete() {
    return selected?.audiobook?.chapters.every((chapter) => isLocalFile(chapter.fileId)) ?? false;
  }

  function selectedAudiobookDownloading() {
    const audiobookId = selected?.audiobook?.audiobookId;
    return Boolean(audiobookId && audiobookDownloads.some((item) => item.audiobookId === audiobookId));
  }

  async function playSelectedAudiobook() {
    const book = selected?.audiobook;
    if (book) await playAudiobook(book);
  }

  async function downloadSelectedAudiobook() {
    const book = selected?.audiobook;
    if (book) await startAudiobookDownload(book);
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
    if (nativeReady && selected?.audiobook?.local) await playSelectedAudio();
    else if (nativeReady && selected && isLocalFile(selected.fileId)) await playSelectedAudio();
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
        activityMessage = msg("File hash blocked locally");
      } else {
        await invoke('block_user', { pubkey: target.pubkey });
        activityMessage = msg("Nostr publisher blocked locally");
      }
      blockConfirmation = null;
      if (activeView === 'Trollbox') {
        trollboxMessages = trollboxMessages.filter((message) => message.pubkey !== ('pubkey' in target ? target.pubkey : ''));
        await refreshTrollbox();
      } else {
        await search();
      }
    } catch (error) {
      activityMessage = msg("Could not block {p0}: {p1}", { p0: target.kind, p1: String(error) });
    } finally {
      blockInProgress = false;
    }
  }

  async function removeTransfer(id: number) {
    if (clearingTransfers || removingTransfers.has(id)) return;
    const target = transfers.find((transfer) => transfer.id === id);
    const fileId = target && isActiveTransfer(target) ? target.fileId : undefined;
    audiobookDownloads = audiobookDownloads.filter((book) => book.activeFileId !== fileId);
    removingTransfers = new Set(removingTransfers).add(id);
    if (fileId) {
      cancellingFiles.add(fileId);
      const attempt = downloadAttempts.get(fileId);
      if (attempt) attempt.cancelled = true;
    }
    const pending = [...pendingDownloadRequests].filter(([, file]) => file === fileId).map(([request]) => request);
    activityMessage = msg("Stopping download and cleaning partial files…");
    try {
      if (nativeReady) {
        // Optimistic UI IDs are not database IDs. Find the actual row, and
        // drain any request still entering the backend before the final sweep.
        const removeRows = async () => {
          const rows = await invoke<NativeTransfer[]>('get_transfers');
          for (const row of rows) {
            if (row.id === id || (pending.length && fileId && row.id < 0 && row.fileId === fileId && isActiveTransfer(row))) {
              await invoke('remove_transfer', { id: row.id });
            }
          }
        };
        await removeRows();
        await Promise.allSettled(pending);
        if (pending.length) await removeRows();
      }
      transfers = transfers.filter((transfer) => transfer.id !== id && !(pending.length && fileId && transfer.fileId === fileId && isActiveTransfer(transfer)));
      activityMessage = msg("Transfer removed; completed audio kept");
    } catch (error) {
      activityMessage = msg("Could not remove transfer: {p0}", { p0: String(error) });
    } finally {
      if (fileId) cancellingFiles.delete(fileId);
      removingTransfers = new Set([...removingTransfers].filter((value) => value !== id));
    }
  }

  async function clearAllTransfers() {
    if (clearingTransfers || removingTransfers.size) return;
    clearingTransfers = true;
    downloadGeneration += 1;
    audiobookDownloads = [];
    const pending = [...pendingDownloadRequests.keys()];
    activityMessage = msg("Stopping downloads and cleaning partial files…");
    try {
      if (nativeReady) {
        await invoke('clear_all_transfers');
        // A request already sent by the UI may still be entering the backend.
        await Promise.allSettled(pending);
        if (pending.length) await invoke('clear_all_transfers');
        transfers = mapTransfers(await invoke<NativeTransfer[]>('get_transfers'));
      } else transfers = [];
      activityMessage = msg("All transfers cleared; partial downloads removed and completed audio kept");
    } catch (error) {
      activityMessage = msg("Could not clear all transfers: {p0}", { p0: String(error) });
    } finally {
      clearingTransfers = false;
    }
  }

  async function clearFinishedTransfers() {
    if (clearingTransfers || removingTransfers.size) return;
    const finished = transfers.filter(isFinishedTransfer);
    if (!finished.length) return;
    const removed = new Set<number>();
    for (const transfer of finished) {
      try {
        if (nativeReady) await invoke('remove_transfer', { id: transfer.id });
        removed.add(transfer.id);
      } catch (error) {
        activityMessage = msg("Could not clear every finished transfer: {p0}", { p0: String(error) });
        break;
      }
    }
    transfers = transfers.filter((transfer) => !removed.has(transfer.id));
    if (removed.size === finished.length) activityMessage = msg("Finished transfers cleared: {p0}", { p0: removed.size });
    if (nativeReady) await refreshLocalLibrary();
  }

  async function togglePause() {
    paused = !paused;
    if (nativeReady) {
      try { await invoke('set_downloads_paused', { paused }); activityMessage = paused ? msg("All active downloads paused") : msg("Downloads resumed"); }
      catch (error) { activityMessage = msg("Could not change download state: {p0}", { p0: String(error) }); }
    }
  }

  async function chooseNapstrFolder() {
    if (!nativeReady) { activityMessage = msg("Folder selection is available in the packaged desktop app"); return; }
    try {
      const selectedPath = await open({ directory: true, multiple: false, title: $t("Choose the folder Napstr uses for downloads and sharing"), defaultPath: napstrFolder || undefined });
      if (!selectedPath || Array.isArray(selectedPath)) return;
      activityMessage = msg("Indexing files and calculating SHA-256 hashes…");
      const report = await invoke<{ fileCount: number; totalBytes: number; errors: string[]; errorCount: number; changedFiles: number }>('set_napstr_folder', { path: selectedPath });
      activityMessage = msg("Indexed {p0} files, {p1}{p2}", { p0: report.fileCount, p1: readableSize(report.totalBytes), p2: report.errorCount ? ` · ${report.errorCount} skipped` : '' });
    } catch (error) { activityMessage = msg("Folder selection failed: {p0}", { p0: String(error) }); }
  }

  async function openNapstrFolder() {
    if (!nativeReady) return;
    try { await invoke('open_napstr_folder'); }
    catch (error) { activityMessage = msg("Could not open Napstr folder: {p0}", { p0: String(error) }); }
  }

  async function rescanSharedFolder() {
    if (!nativeReady || rescanPending) return;
    rescanPending = true;
    activityMessage = msg("Rescanning Napstr folder…");
    try {
      const report = await invoke<{ fileCount: number; totalBytes: number; changedFiles: number }>('rescan_napstr_folder');
      activityMessage = msg("Indexed {p0} files, {p1}", { p0: report.fileCount, p1: readableSize(report.totalBytes) });
    } catch (error) {
      activityMessage = msg("Rescan failed: {p0}", { p0: String(error) });
    } finally {
      rescanPending = false;
    }
  }

  async function cancelLibraryScan() {
    if (!nativeReady || !indexing) return;
    try {
      await invoke('cancel_library_scan');
      activityMessage = msg("Cancelling the library scan…");
    } catch (error) {
      activityMessage = msg("Could not cancel indexing: {p0}", { p0: String(error) });
    }
  }

  async function persistSettings() {
    if (!nativeReady) return;
    try {
      applySnapshot(await invoke<Snapshot>('save_settings', { settings: { napstrFolder, nostrRelays, displayName, profileAbout, profilePicture } }));
      if (networkConnected) await invoke('publish_profile');
      activityMessage = networkConnected ? msg("Settings saved and profile published") : msg("Settings saved");
    } catch (error) { activityMessage = msg("Could not save settings: {p0}", { p0: String(error) }); }
  }

  const windowCommand = async (command: 'minimise_window' | 'toggle_maximise' | 'close_window') => {
    if (nativeReady) await invoke(command);
  };

  async function toggleCompactMode() {
    if (!nativeReady || compactModePending) return;
    compactModePending = true;
    const compact = !compactMode;
    try {
      await invoke('set_compact_mode', { compact });
      compactMode = compact;
    } catch (error) {
      activityMessage = msg("Could not switch window mode: {p0}", { p0: String(error) });
    } finally {
      compactModePending = false;
    }
  }

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

  onMount(() => initializeLocale(() => osLocale()));

  onMount(() => {
    desktopRuntime = '__TAURI_INTERNALS__' in window;
    if (!desktopRuntime) return;
    const savedPlayerMode = readStoredPreference('napstr-player-mode');
    if (savedPlayerMode === 'single' || savedPlayerMode === 'folder' || savedPlayerMode === 'all') playerMode = savedPlayerMode;
    const savedPlayerVolume = Number(readStoredPreference('napstr-player-volume'));
    if (Number.isFinite(savedPlayerVolume) && savedPlayerVolume >= 0 && savedPlayerVolume <= 1) playerVolume = savedPlayerVolume;
    const savedTransferHeight = Number(readStoredPreference('napstr-transfer-pane-height'));
    setTransferPaneHeight(Number.isFinite(savedTransferHeight) && savedTransferHeight > 0 ? savedTransferHeight : window.innerHeight < 700 ? 94 : 119);
    const clampTransferPane = () => setTransferPaneHeight(transferPaneHeight);
    window.addEventListener('resize', clampTransferPane);
    refreshSnapshot().then(connectNetwork);
    void getVersion()
      .then((version) => {
        appVersion = version;
        return checkForNewRelease();
      })
      .catch(() => {
        appVersion = 'unknown';
      });
    let destroyed = false;
    const eventUnlisteners: UnlistenFn[] = [];
    void listen<string>('napstr-public-chat', ({ payload: topic }) => {
      if (topic === 'napstr-trollbox') void refreshTrollbox();
      const fileId = selected?.fileId?.toLowerCase();
      if (fileId && topic === `napstr-${fileId}`) void refreshTrackDiscussion(fileId);
    }).then((unlisten) => {
      if (destroyed) unlisten();
      else eventUnlisteners.push(unlisten);
    });
    void listen('napstr-library-changed', () => {
      void refreshLocalLibrary();
    }).then((unlisten) => {
      if (destroyed) unlisten();
      else eventUnlisteners.push(unlisten);
    });
    void listen('napstr-transfers-changed', () => {
      void invoke<NativeTransfer[]>('get_transfers')
        .then(async (items) => {
          if (clearingTransfers || removingTransfers.size) return;
          await applyTransferUpdate(items);
          if (audiobookDownloads.length) await advanceAudiobookDownloads();
        })
        .catch(() => {});
    }).then((unlisten) => {
      if (destroyed) unlisten();
      else eventUnlisteners.push(unlisten);
    });
    void listen<IndexBatch>('napstr-index-batch', ({ payload }) => {
      mergeIndexBatch(payload);
    }).then((unlisten) => {
      if (destroyed) unlisten();
      else eventUnlisteners.push(unlisten);
    });
    void listen<IndexProgress>('napstr-index-progress', ({ payload }) => {
      indexing = payload.scanning;
      if (payload.message) activityMessage = payload.message;
      if (!payload.scanning) rescanPending = false;
    }).then((unlisten) => {
      if (destroyed) unlisten();
      else eventUnlisteners.push(unlisten);
    });
    const updateClock = () => {
      clockNow = Date.now();
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
          activityMessage = msg("Tor failed: {p0} · click the connection panel to retry", { p0: status.torError });
        } else if (!wasConnected && status.connected) {
          activityMessage = msg("Nostr reconnected · refreshing the catalogue");
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
      const transferWorkPending =
        downloadLibraryRefreshNeeded ||
        startingDownloads.size > 0 ||
        audiobookDownloads.length > 0 ||
        transfers.some(isActiveTransfer);
      if (!nativeReady || transferPollPending || !transferWorkPending || clearingTransfers || removingTransfers.size) return;
      transferPollPending = true;
      try {
        const items = await invoke<NativeTransfer[]>('get_transfers');
        if (clearingTransfers || removingTransfers.size) return;
        await applyTransferUpdate(items);
        if (audiobookDownloads.length) await advanceAudiobookDownloads();
      } catch { /* the next transfer poll retries */ }
      finally { transferPollPending = false; }
    }, 1000);
    const playerTimer = window.setInterval(() => {
      if (!nativeReady || !currentTrack || playerLoading) return;
      invoke<PlaybackStatus>('audio_status').then((status) => {
        const naturallyEnded = status.fileId === currentTrack?.fileId && status.ended && !playerEnded;
        applyPlaybackStatus(status);
        if (naturallyEnded) void playerTrackEnded();
      }).catch(() => {});
    }, 250);
    const mobileTimer = window.setInterval(() => {
      if (mobilePairing && mobilePairing.expiresAt <= Math.floor(Date.now() / 1000)) mobilePairing = null;
      if (mobileStreamPairing && mobileStreamPairing.expiresAt <= Math.floor(Date.now() / 1000)) mobileStreamPairing = null;
      if (activeView === 'Napstrfy') void refreshMobileStatus();
    }, 3000);
    return () => {
      destroyed = true;
      eventUnlisteners.forEach((unlisten) => unlisten());
      clearInterval(clockTimer);
      clearInterval(wakeTimer);
      clearInterval(networkTimer);
      clearInterval(transferTimer);
      clearInterval(playerTimer);
      clearInterval(mobileTimer);
      window.removeEventListener('resize', clampTransferPane);
      window.removeEventListener('focus', foregrounded);
      document.removeEventListener('visibilitychange', foregrounded);
      stopTransferResize();
      if (nativeReady && currentTrack) invoke<PlaybackStatus>('stop_audio').catch(() => {});
    };
  });
</script>

<svelte:head><title>{$t("Napstr - own your music again")}</title></svelte:head>

{#if desktopRuntime}
<main class="desktop">
  <section class:compact={compactMode} class="app-window" style={`--transfer-height: ${transferPaneHeight}px`} aria-label={$t("Napstr application window")}>
    <button class="window-resize-handle resize-n" aria-label={$t("Resize window from top")} onpointerdown={(event) => beginWindowResize(event, 'North')}></button>
    <button class="window-resize-handle resize-e" aria-label={$t("Resize window from right")} onpointerdown={(event) => beginWindowResize(event, 'East')}></button>
    <button class="window-resize-handle resize-s" aria-label={$t("Resize window from bottom")} onpointerdown={(event) => beginWindowResize(event, 'South')}></button>
    <button class="window-resize-handle resize-w" aria-label={$t("Resize window from left")} onpointerdown={(event) => beginWindowResize(event, 'West')}></button>
    <button class="window-resize-handle resize-ne" aria-label={$t("Resize window from top right")} onpointerdown={(event) => beginWindowResize(event, 'NorthEast')}></button>
    <button class="window-resize-handle resize-se" aria-label={$t("Resize window from bottom right")} onpointerdown={(event) => beginWindowResize(event, 'SouthEast')}></button>
    <button class="window-resize-handle resize-sw" aria-label={$t("Resize window from bottom left")} onpointerdown={(event) => beginWindowResize(event, 'SouthWest')}></button>
    <button class="window-resize-handle resize-nw" aria-label={$t("Resize window from top left")} onpointerdown={(event) => beginWindowResize(event, 'NorthWest')}></button>

    <header class="titlebar" data-tauri-drag-region>
      <div class="title-left"><span class="app-icon"><img src="/napstr-logo.png" alt="" /></span><span>{compactMode ? (currentTrack?.name ?? $t("Napstr audio player")) : $t("Napstr - own your music again")}</span></div>
      <div class="title-actions">
        <button class="compact-toggle" disabled={compactModePending} onclick={toggleCompactMode} aria-label={compactMode ? $t("Restore full window") : $t("Switch to compact player")} title={compactMode ? $t("Restore full window") : $t("Switch to compact player")}>{compactMode ? '↗' : '▁'}</button>
        <div class="window-controls" aria-hidden="true">
          <button tabindex="-1" onclick={() => windowCommand('minimise_window')}>_</button>{#if !compactMode}<button tabindex="-1" onclick={() => windowCommand('toggle_maximise')}>□</button>{/if}<button tabindex="-1" onclick={() => windowCommand('close_window')}>×</button>
        </div>
      </div>
    </header>

    <div class="toolbar">
      <div class="toolbar-brand" title={$t("Napstr home")}>
        <img src="/napstr-logo.png" alt="Napstr" />
      </div>
      <div class="toolbar-separator"></div>
      {#each views as view}
        <button class:active={activeView === view.label} class="tool-button" onclick={() => activateView(view.label)}>
          <span class="tool-icon icon-{view.label.toLowerCase()}">{view.icon}</span>
          <span>{$t(view.label)}</span>
        </button>
      {/each}
      <div class="toolbar-spacer"></div>
      {#if newRelease}
        <button class="release-button" onclick={openNewRelease} title={$t("Open Napstr {p0} on GitHub", { p0: newRelease.version })}>
          <span class="release-arrow">⇧</span>
          <span><strong>{$t("New release")}</strong><small>{newRelease.version} {$t("available")}</small></span>
        </button>
      {/if}
      <button class="connection-box" onclick={connectNetwork} title={torError || networkError || $t("Reconnect Nostr and Tor")}>
        <span class="connection-status"><i class:amber={!networkConnected} class="led"></i><strong>{networkConnected ? $t("Nostr connected") : $t("Connect Nostr")}</strong></span>
        <span class="connection-status"><i class:amber={!torRunning} class:error={Boolean(torError)} class="led"></i><strong>{$t(torStatusLabel())}</strong></span>
      </button>
      <button class="tool-button help-button" onclick={() => (aboutOpen = true)}><span class="tool-icon">?</span><span>{$t("About")}</span></button>
    </div>

    <div class="network-strip">
      <span class="network-pulse">▥</span>
      <span>{$t(activityMessage)}</span>
      <span class="strip-right"><button class="user-name" disabled={!identityNpub} onclick={() => browseUser(ownCatalogueUser())}>{displayName}</button> <i class:amber={!nativeReady} class="led"></i></span>
    </div>

    <section class="player-bar" aria-label={$t("Napstr audio player")}>
      <div class="player-display">
        <span class:playing={playerPlaying} class="player-led">{playerLoading ? '···' : playerPlaying ? '▶' : '■'}</span>
        <div><strong>{currentTrack?.name ?? $t("No track selected")}</strong><small>{currentTrack ? `${currentTrack.artist || 'Unknown artist'} · ${folderName(currentTrack.folder)}` : $t("Choose a local song to begin")}</small></div>
      </div>
      <div class="player-controls">
        <button onclick={previousPlayerTrack} disabled={!currentTrack || playerLoading} title={$t("Previous track")}>|◀</button>
        <button class="player-primary" onclick={togglePlayer} disabled={playerLoading} title={playerPlaying ? $t("Pause") : $t("Play")}>{playerLoading ? '…' : playerPlaying ? 'Ⅱ' : '▶'}</button>
        <button onclick={stopPlayer} disabled={!currentTrack || playerLoading} title={$t("Stop")}>■</button>
        <button onclick={nextPlayerTrack} disabled={playerLoading || playerQueueIndex < 0 || playerQueueIndex + 1 >= playerQueue.length} title={$t("Next track")}>▶|</button>
      </div>
      <div class="player-seek">
        <input aria-label={$t("Track position")} type="range" min="0" max={Math.max(0, playerDuration || 0)} step="0.1" value={playerCurrentTime} oninput={seekPlayer} disabled={!currentTrack} />
        <span>{formatPlayerTime(playerCurrentTime)} / {formatPlayerTime(playerDuration)}</span>
      </div>
      <label class="player-mode">{$t("After track")}
        <select bind:value={playerMode} onchange={changePlayerMode}>
          <option value="single">{$t("Stop")}</option>
          <option value="folder">{$t("Play folder")}</option>
          <option value="all">{$t("Play all")}</option>
        </select>
      </label>
      <label class="player-volume">{$t("Vol")} <input aria-label={$t("Volume")} type="range" min="0" max="1" step="0.05" value={playerVolume} oninput={changePlayerVolume} /></label>
    </section>

    <div class="workspace">
      {#if activeView === 'Search'}
        <section class="panel search-panel">
          <div class="panel-title"><span></span><b>{$t("Search the Napstr network")}</b><span></span></div>
          <form class="search-form" onsubmit={(e) => { e.preventDefault(); search(); }}>
            <label for="search-query">{$t("Search:")}</label>
            <input id="search-query" bind:value={query} placeholder={$t("punk, rock, jazz, audiobook")} />
            <label for="format">{$t("File type:")}</label>
            <select id="format" bind:value={format} disabled={searchAction !== null} onchange={() => void search()}><option value="Audio only">{$t("Audio only")}</option><option value="Audiobooks">{$t("Audiobooks")}</option></select>
            <button class="classic-button primary search-button" type="submit" disabled={searchAction !== null} aria-busy={searchAction === 'search'}>
              {#if searchAction === 'search'}<span class="search-spinner" aria-hidden="true"></span>{/if}
              {searchAction === 'search' ? $t("Searching") : $t("Search")}
            </button>
            <button class="classic-button surprise-button" type="button" onclick={surpriseMe} disabled={searchAction !== null || !networkConnected} aria-busy={searchAction === 'surprise'}>
              {#if searchAction === 'surprise'}<span class="search-spinner" aria-hidden="true"></span>{/if}
              {searchAction === 'surprise' ? $t("Choosing…") : $t("Surprise me")}
            </button>
          </form>
          {#if resultUser}
            <div class="user-search-status"><span>{$t("Shared by")} <b>{resultUser.displayName}</b> <code title={resultUser.npub}>{resultUser.npub.slice(0, 18)}…</code></span><button class="classic-button" disabled={searchAction !== null} onclick={() => { query = ''; searchUser = null; void search(); }}>{$t("Clear user filter")}</button></div>
          {:else if matchingUsers.length > 1}
            <div class="user-search-status"><span>{$t("Several users have this name. Choose whose songs to browse:")}</span>{#each matchingUsers as user}<button class="classic-button" title={user.npub} onclick={() => browseUser(user)}>{user.displayName} · {user.npub.slice(0, 18)}…</button>{/each}</div>
          {/if}
          <button class="advanced-toggle" onclick={() => (advanced = !advanced)}><span>{advanced ? '▼' : '▶'}</span> {advanced ? $t("Hide advanced search options") : $t("Show advanced search options")}</button>
          {#if advanced}
            <div class="advanced-row"><label>{$t("Minimum seeders:")} <input type="number" bind:value={minimumSources} min="1" /></label><label>{$t("Maximum size:")} <input bind:value={maximumSize} placeholder={$t("e.g. 2 GB")} /></label><label><input type="checkbox" checked disabled /> {$t("Online seeders only")}</label></div>
          {/if}
        </section>

        <div class="split-content">
          <section class="results-pane" aria-label={$t("Search results")}>
            <div class="section-caption"><span>{$t("Results for “{p0}”", { p0: ["All audio", "All audiobooks", "local catalogue", "Surprise me"].includes(searchedQuery) ? $t(searchedQuery) : searchedQuery })}</span><small>{format === 'Audiobooks' ? $t("Audiobooks found: {p0}", { p0: results.length }) : browseTotalAvailable ? $t("{p0} loaded of {p1} available", { p0: results.length, p1: availableResultTotal() }) : $t("{p0} file IDs found", { p0: results.length })}</small></div>
            <div class="table-wrap">
              <table class="file-table search-results-table">
                <thead><tr><th class="name-col">{$t("Name")}</th><th>{$t("Type")}</th><th class="number">{$t("Size")}</th><th class="number">{$t("Seeders")}</th><th>{$t("Line speed")}</th><th>{$t("Length")}</th></tr></thead>
                <tbody>
                  {#each paginatedResults() as item}
                    <tr class:selected={selectedResultIds.has(item.fileId)} aria-selected={selectedResultIds.has(item.fileId)} tabindex="0"
                      onclick={(event) => selectResultRange(item, event)}
                      onkeydown={(event) => { if (event.key === ' ' || event.key === 'Enter') { event.preventDefault(); selectResultRange(item, event); } }}
                      ondblclick={(event) => { if (!event.shiftKey) void activateSelected(); }}>
                      <td><span class:audiobook-icon={Boolean(item.audiobook)} class="file-icon">{item.audiobook ? '▥' : '▶'}</span>{item.name}</td><td>{item.format}</td><td class="number">{item.size}</td><td class="number"><span class="source-dot"></span>{item.sources}</td><td>{$t(item.speed)}</td><td>{item.length}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
            <div class="results-pager">
              <button onclick={() => void changeResultPage(resultPage - 1)} disabled={resultPage === 0}>{$t("◀ Previous")}</button>
              <span>{$t("Showing {range} of {count} loaded", { range: resultRange(), count: results.length })}{browseTotalAvailable ? $t(" · {p0} available", { p0: availableResultTotal() }) : ''} · {$t("Page {page} of {pages}", { page: resultPage + 1, pages: resultPageCount() })}{browseCursor ? '+' : ''}</span>
              <button onclick={() => void changeResultPage(resultPage + 1)} disabled={browseLoading || (resultPage + 1 >= resultPageCount() && !browseCursor)}>{browseLoading ? $t("Loading…") : $t("Next ▶")}</button>
            </div>
          </section>

          <aside class="details-pane">
            <div class="section-caption"><span>{$t("File details")}</span></div>
            {#if selected}
              {#if selectedResultIds.size > 1}
                <div class="selected-file">
                  <div class="large-file-icon">♫</div>
                  <div><strong>{$t("Selected tracks: {p0}", { p0: selectedResultIds.size })}</strong><span>{$t("Click a track, then Shift-click another to select a range.")}</span></div>
                </div>
                <div class="detail-actions"><button class="classic-button primary" onclick={downloadSelectedResults} disabled={!nativeReady || downloadingSelection || !selectedResults().some(canDownloadResult)} aria-busy={downloadingSelection}>{downloadingSelection ? $t("… Requesting") : $t("⇩ Download All")}</button></div>
                <p class="privacy-note"><span>♜</span> {$t("Downloads use Tor. Tracks already on this computer, queued, or without seeders are skipped.")}</p>
              {:else if selected.audiobook}
                <div class="selected-file audiobook-selected">
                  <div class="large-file-icon">▥</div>
                  <div><strong>{selected.audiobook.title}</strong><span>{$t("Audiobook ·")} {selected.audiobook.chapters.length} {$t("chapters ·")} {selected.size}</span><small>{$t("Edition ID:")} {selected.audiobook.audiobookId}</small></div>
                </div>
                <div class="file-metadata"><p><b>{selected.audiobook.author || $t("Unknown author")}</b>{selected.audiobook.narrator ? $t(" · Narrated by {p0}", { p0: selected.audiobook.narrator }) : ''}</p><small>{$t("Chapters are ordered and each file is independently SHA-256 verified.")}</small></div>
                <div class="audiobook-chapters" aria-label={$t("Audiobook chapters")}>
                  {#each selected.audiobook.chapters as chapter}
                    <button
                      type="button"
                      class:chapter-local={isLocalFile(chapter.fileId)}
                      class:chapter-playing={currentTrack?.fileId === chapter.fileId}
                      disabled={!isLocalFile(chapter.fileId)}
                      title={isLocalFile(chapter.fileId) ? $t("Play {p0}", { p0: chapter.title }) : $t("{p0} has not downloaded yet", { p0: chapter.title })}
                      onclick={() => playAudiobookChapter(selected!.audiobook!, chapter.fileId)}
                    ><span>{String(chapter.position).padStart(2, '0')}</span><b>{chapter.title}</b><small>{readableSize(chapter.size)}</small><i>{$t(audiobookChapterStatus(selected.audiobook!, chapter))}</i></button>
                  {/each}
                </div>
                <div class="detail-actions">{#if selectedAudiobookComplete()}<button class="classic-button primary" onclick={playSelectedAudiobook}>{$t("▶ Play book")}</button><button class="classic-button" onclick={openNapstrFolder}>{$t("Open folder")}</button>{:else}<button class="classic-button primary" disabled={selectedAudiobookDownloading()} onclick={downloadSelectedAudiobook}>⇩ {selectedAudiobookDownloading() ? $t("Downloading…") : $t("Download book")}</button>{/if}</div>
                {#if !selected.audiobook.local}<p class="privacy-note"><span>♜</span> {$t("Chapters download first-to-last through private Tor onion services. Play each chapter as soon as it shows Ready.")}</p>{:else}<p class="privacy-note"><span>♬</span> {$t("This complete audiobook is ready to play.")}</p>{/if}
              {:else}
              <div class="selected-file">
                <div class="large-file-icon">▶</div>
                <div><strong>{selected.name}</strong><span>{selected.format} · {selected.size} · {selected.length}</span><small>{$t("File ID:")} {selected.fileId}</small></div>
              </div>
              {#if selected.artist || selected.album}<div class="file-metadata"><small>{selected.artist ? $t("Artist: {p0}", { p0: selected.artist }) : ''}{selected.artist && selected.album ? ' · ' : ''}{selected.album ? $t("Album: {p0}", { p0: selected.album }) : ''}</small></div>{/if}
              {#if selected.tags}<div class="file-metadata"><small>{$t("Tags:")} {selected.tags}</small></div>{/if}
              <fieldset><legend>{$t("Seeders")}</legend>
                <div class="sources-list">
                  {#if !isLocalFile(selected.fileId)}
                    {#each (selected.sourceDetails ?? []).slice(0, VISIBLE_SEEDER_LIMIT) as source, index}
                      <div class:selected-source={selectedSource === index} class="source-row"><button class="user-icon source-select" title={$t("Select {p0} for profile and moderation actions", { p0: source.displayName })} onclick={() => (selectedSource = index)}>☺</button><button class="user-name" title={$t("Browse songs shared by {p0}", { p0: source.displayName })} onclick={() => browseUser(source)}>{source.displayName}</button><small>{source.npub.slice(0, 12)}…</small><span class="online"><i></i> {$t("Seeding")}</span></div>
                    {/each}
                  {:else}
                    <div><span class="user-icon">☺</span><b>{$t("This computer")}</b><small>{$t("Local")}</small><span class="online"><i></i> {$t("Ready")}</span></div>
                  {/if}
                </div>
              </fieldset>
              <div class="detail-actions">{#if !isLocalFile(selected.fileId)}<button class="classic-button primary" disabled={startingDownloads.has(selected.fileId)} onclick={() => startDownload()}>{startingDownloads.has(selected.fileId) ? $t("… Requesting") : $t("⇩ Download")}</button><button class="classic-button" onclick={() => (sourceProfile = selected?.sourceDetails?.[selectedSource] ?? null)}>{$t("View profile")}</button>{:else}<button class="classic-button primary" onclick={playSelectedAudio}>{$t("▶ Play")}</button><button class="classic-button" onclick={openNapstrFolder}>{$t("Open folder")}</button>{/if}</div>
              {#if !isLocalFile(selected.fileId)}<div class="detail-actions moderation-actions"><button class="classic-button" onclick={blockSelectedFile}>{$t("Block file")}</button><button class="classic-button" onclick={blockSelectedUser}>{$t("Block user")}</button></div>{/if}
              {#if !isLocalFile(selected.fileId)}<p class="privacy-note"><span>♜</span> {$t("Transfer will use the seeder’s private, app-session Tor onion service.")}</p>{:else}<p class="privacy-note"><span>♬</span> {$t("Downloaded and verified · ready to play from your Napstr folder.")}</p>{/if}
              <section class="track-discussion" aria-label={$t("Discussion for {p0}", { p0: selected.name })}>
                <div class="track-discussion-title"><b>{$t("Track discussion")}</b><small>{$t(trackDiscussionLoading ? "Loading…" : "Public · Nostr")}</small></div>
                <div class="track-discussion-log" use:openChatLog bind:this={trackDiscussionLog} aria-live="polite" aria-busy={trackDiscussionLoading} onscroll={(event) => { if (event.currentTarget.scrollTop < 32) void refreshTrackDiscussion(selected?.fileId, false, true); }}>
                  {#if trackDiscussionLoading && !trackDiscussionMessages.length}<p class="trollbox-notice">{$t("Loading comments…")}</p>{/if}
                  {#if !trackDiscussionLoading && trackDiscussionMessages.length === 0 && !trackDiscussionError}<p class="trollbox-notice">{$t("No comments yet.")}</p>{/if}
                  {#each trackDiscussionMessages as message (message.eventId)}
                    <div class="trollbox-message"><button class="trollbox-name" style:color={chatNameColor(message.npub)} title={$t("Browse songs shared by {p0} · {p1}", { p0: message.displayName, p1: message.npub })} onclick={() => browseUser(message)}>{message.displayName}:</button><span>{message.content}</span>{#if message.npub !== identityNpub}<button class="chat-block" aria-label={$t("Block {p0}", { p0: message.displayName })} onclick={() => blockTrollboxUser(message)}>{$t("Block")}</button>{/if}</div>
                  {/each}
                </div>
                {#if trackDiscussionError}<div class="track-discussion-error">{trackDiscussionError}</div>{/if}
                <div class="track-discussion-compose">
                  <input bind:value={trackDiscussionDraft} maxlength="500" autocomplete="off" placeholder={networkConnected ? $t("Comment on this track…") : $t("Connect to Nostr to comment")} disabled={!networkConnected || trackDiscussionSending} aria-label={$t("Track discussion comment")} onkeydown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void sendTrackDiscussionMessage(); } }} />
                  <button class="classic-button primary" type="button" disabled={!networkConnected || trackDiscussionSending || !trackDiscussionDraft.trim()} onclick={() => void sendTrackDiscussionMessage()}>{trackDiscussionSending ? '…' : $t("Send")}</button>
                </div>
              </section>
              {/if}
            {:else}<p class="empty-state">{$t("Select a result to see active seeders.")}</p>{/if}
          </aside>
        </div>
      {:else if activeView === 'Downloads'}
        <section class="full-panel downloads-view">
          <div class="panel-title"><span></span><b>{$t("Download Manager")}</b><span></span></div>
          <div class="actionbar"><button class="classic-button" onclick={togglePause}>{paused ? $t("▶ Resume all") : $t("Ⅱ Pause all")}</button><button class="classic-button" onclick={openNapstrFolder}>{$t("Open Napstr folder")}</button><button class="classic-button" onclick={clearFinishedTransfers} disabled={clearingTransfers || !transfers.some(isFinishedTransfer)}>{$t("Clear finished")}</button><button class="classic-button" onclick={clearAllTransfers} disabled={clearingTransfers || removingTransfers.size > 0 || (!transfers.length && !audiobookDownloads.length && !startingDownloads.size)}>{clearingTransfers ? $t("Clearing…") : $t("Clear all")}</button><div class="spacer"></div><span>{transfers.filter(isActiveTransfer).length} {$t("active ·")} {transfers.filter(isCompleteTransfer).length} {$t("ready to play")}</span></div>
          <div class="download-queue">
            {#each audiobookDownloads as book}
              <div class="audiobook-download-row"><span class="audiobook-glyph">▥</span><b>{book.title}</b><div class="progress"><span style={`width:${book.chapters.length ? (book.nextIndex / book.chapters.length) * 100 : 0}%`}></span><b>{book.nextIndex}/{book.chapters.length}</b></div><span>{book.activeFileId ? $t("Downloading chapter {p0}", { p0: book.nextIndex + 1 }) : $t("Preparing next chapter")}</span></div>
            {/each}
            <table class="file-table download-table"><thead><tr><th>{$t("Download order")}</th><th>{$t("Progress")}</th><th>{$t("Size")}</th><th>{$t("Speed")}</th><th>{$t("Status")}</th><th></th></tr></thead><tbody>
              {#each transfers as transfer}
                <tr class:transfer-complete={isCompleteTransfer(transfer)} ondblclick={() => { if (isCompleteTransfer(transfer)) playAudio(transfer.fileId, transfer.name, playerMode, 'downloads'); }}><td><span class="download-arrow">{isCompleteTransfer(transfer) ? '▶' : '⇩'}</span>{transfer.name}</td><td><div class="progress"><span style={`width:${transfer.progress}%`}></span><b>{Math.round(transfer.progress)}%</b></div></td><td>{transfer.size}</td><td>{isCompleteTransfer(transfer) ? $t("Local") : transfer.speed}</td><td>{isCompleteTransfer(transfer) ? $t("Ready to play") : transfer.status}</td><td class="transfer-actions">{#if isCompleteTransfer(transfer)}<button class="classic-button transfer-play" onclick={(event) => { event.stopPropagation(); playAudio(transfer.fileId, transfer.name, playerMode, 'downloads'); }} title={$t("Play verified audio")}>{$t("▶ Play")}</button>{/if}<button class="tiny-button" disabled={clearingTransfers || removingTransfers.has(transfer.id)} onclick={(event) => { event.stopPropagation(); removeTransfer(transfer.id); }} aria-label={$t("Remove transfer: {p0}", { p0: transfer.name })} title={$t("Cancel and clear entry; keep completed audio")}>×</button></td></tr>
              {/each}
            </tbody></table>
            {#if transfers.length === 0}<p class="empty-state compact">{$t("There are no downloads in the queue.")}</p>{/if}
          </div>
          <div class="panel-title"><span></span><b>{$t("Track Tags")}</b><span></span></div>
          <div class="tag-editor">
            <b>{selectedTagFile?.filename ?? $t("Select a local track below")}</b>
            <input bind:value={tagDraft} disabled={!selectedTagFile || tagSaving} maxlength="256" placeholder={$t("punk, live, audiobook")} onkeydown={(event) => { if (event.key === 'Enter') saveTags(); }} />
            <button class="classic-button primary" onclick={saveTags} disabled={!selectedTagFile || tagSaving}>{tagSaving ? $t("Saving…") : $t("Save tags")}</button>
            <small>{$t("Comma-separated · published with your signed catalogue")}</small>
          </div>
          <div class="tag-library">
            <table class="file-table tags-table"><thead><tr><th>{$t("Name")}</th><th>{$t("Folder")}</th><th>{$t("Tags")}</th></tr></thead><tbody>
              {#each paginatedTagFiles() as file}
                <tr class:selected={selectedTagFile?.fileId === file.fileId} onclick={() => selectTagFile(file)} ondblclick={() => playAudio(file.fileId, file.filename, playerMode, 'downloads')}><td><button type="button" class="file-icon file-play-button" title={$t("Play {p0}", { p0: file.filename })} aria-label={$t("Play {p0}", { p0: file.filename })} onclick={(event) => { event.stopPropagation(); selectTagFile(file); playAudio(file.fileId, file.filename, playerMode, 'downloads'); }}>▶</button>{file.filename}</td><td>{folderName(file.folder)}</td><td>{file.tags || '—'}</td></tr>
              {/each}
            </tbody></table>
            {#if sharedFiles.length === 0}<p class="empty-state compact">{$t("Downloaded and shared tracks will appear here.")}</p>{/if}
          </div>
          {#if sharedFiles.length > LOCAL_PAGE_SIZE}<div class="results-pager"><button disabled={downloadLibraryPage === 0} onclick={() => changeDownloadLibraryPage(downloadLibraryPage - 1)}>{$t("◀ Previous")}</button><span>{$t("Showing {range} of {count} loaded", { range: localPageRange(downloadLibraryPage, sharedFiles.length), count: sharedFiles.length })} · {$t("Page {page} of {pages}", { page: downloadLibraryPage + 1, pages: localPageCount(sharedFiles) })}</span><button disabled={downloadLibraryPage + 1 >= localPageCount(sharedFiles)} onclick={() => changeDownloadLibraryPage(downloadLibraryPage + 1)}>{$t("Next ▶")}</button></div>{/if}
        </section>
      {:else if activeView === 'Shared'}
        <section class="full-panel">
          <div class="panel-title"><span></span><b>{$t("My Shared Files")}</b><span></span></div>
          <div class="actionbar"><button class="classic-button" onclick={indexing ? cancelLibraryScan : rescanSharedFolder}>{indexing ? $t("× Cancel scan") : rescanPending ? $t("… Rescanning") : $t("↻ Rescan")}</button><button class="classic-button" onclick={openNapstrFolder}>{$t("Open folder")}</button><button class="classic-button" onclick={playSelectedSharedAudio} disabled={!selectedShared}>{$t("▶ Play")}</button><button class="classic-button" onclick={playSelectedFolder} disabled={!selectedShared}>{$t("▶ Play folder")}</button><button class="classic-button primary" onclick={playAllSongs} disabled={!sharedFiles.length}>{$t("▶ Play all")}</button><button class="classic-button audiobook-button" onclick={openAudiobookEditor} disabled={libraryFolderView === '*' || audiobookFolderFiles().length < 1}>▥ {currentFolderAudiobook() ? $t("Edit audiobook") : $t("Group as audiobook…")}</button><div class="spacer"></div><span>{$t("Sharing")} {sharedFiles.length} {$t("files ·")} {readableSize(indexedBytes)}</span></div>
          <div class="folder-path"><b>{$t("Napstr folder:")}</b><input value={napstrFolder || 'No folder selected'} readonly /><button class="classic-button" onclick={chooseNapstrFolder}>{$t("Browse…")}</button></div>
          <div class="library-filter">
            <span class="library-filter-label">{$t("View folder:")}</span>
            <div class="folder-picker" use:containLibraryFolderMenu>
              <button type="button" class="folder-picker-toggle" aria-haspopup="listbox" aria-expanded={libraryFolderMenuOpen} onclick={() => (libraryFolderMenuOpen = !libraryFolderMenuOpen)} title={libraryFolderView === '*' ? $t("All folders") : folderName(libraryFolderView)}>
                <span>{libraryFolderView === '*' ? $t("All folders") : folderName(libraryFolderView)}</span><i aria-hidden="true">▼</i>
              </button>
              {#if libraryFolderMenuOpen}
                <div class="folder-picker-menu" role="listbox" aria-label={$t("View folder")}>
                  <button type="button" role="option" aria-selected={libraryFolderView === '*'} class:selected={libraryFolderView === '*'} onclick={() => selectLibraryFolder('*')}>{$t("All folders")}</button>
                  {#each libraryFolders() as folder}
                    <button type="button" role="option" aria-selected={libraryFolderView === folder} class:selected={libraryFolderView === folder} onclick={() => selectLibraryFolder(folder)} title={folderName(folder)}>{folderName(folder)}</button>
                  {/each}
                </div>
              {/if}
            </div>
            <span class="library-song-count">{$t("Songs shown: {p0}", { p0: visibleSharedFiles().length })}</span>
          </div>
          {#if !currentFolderAudiobook() && libraryFolderView.toLowerCase().includes('audiobook') && audiobookFolderFiles().length >= 1}<div class="audiobook-folder-banner"><span class="audiobook-glyph">▥</span><div><b>{$t("Possible audiobook detected")}</b><small>{$t("Review the natural chapter order before making the collection public.")}</small></div><button class="classic-button primary" onclick={openAudiobookEditor}>{$t("Group as audiobook…")}</button></div>{/if}
          {#if currentFolderAudiobook()}<div class="audiobook-folder-banner"><span class="audiobook-glyph">▥</span><div><b>{currentFolderAudiobook()?.title}</b><small>{currentFolderAudiobook()?.author || $t("Unknown author")} · {currentFolderAudiobook()?.chapters.length} {$t("ordered chapters · published as one audiobook")}</small></div><button class="classic-button primary" onclick={() => playAudiobook(currentFolderAudiobook()!)}>{$t("▶ Play book")}</button></div>{/if}
          <table class="file-table shared-table"><thead><tr><th>{$t("Name")}</th><th>{$t("Folder")}</th><th>{$t("Size")}</th><th>{$t("Catalogue")}</th><th>{$t("Active peers")}</th></tr></thead><tbody>{#each paginatedSharedFiles() as file}<tr class:selected={selectedShared?.fileId === file.fileId} onclick={() => (selectedShared = { ...file })} ondblclick={() => playAudio(file.fileId, file.name, playerMode, 'shared')}><td><span class="file-icon">▶</span>{file.name}</td><td>{folderName(file.folder)}</td><td>{file.readableSize}</td><td><span class:amber={!networkConnected} class="led"></span>{networkConnected ? $t("Published") : $t("Indexed")}</td><td>{file.peers}</td></tr>{/each}</tbody></table>
          {#if visibleSharedFiles().length > LOCAL_PAGE_SIZE}<div class="results-pager"><button disabled={sharedLibraryPage === 0} onclick={() => changeSharedLibraryPage(sharedLibraryPage - 1)}>{$t("◀ Previous")}</button><span>{$t("Showing {range} of {count} loaded", { range: localPageRange(sharedLibraryPage, visibleSharedFiles().length), count: visibleSharedFiles().length })} · {$t("Page {page} of {pages}", { page: sharedLibraryPage + 1, pages: localPageCount(visibleSharedFiles()) })}</span><button disabled={sharedLibraryPage + 1 >= localPageCount(visibleSharedFiles())} onclick={() => changeSharedLibraryPage(sharedLibraryPage + 1)}>{$t("Next ▶")}</button></div>{/if}
          <p class="privacy-note wide"><span>♜</span> {$t("Only validated MP3, FLAC, WAV, Ogg Vorbis, and Opus audio is indexed recursively. Put book folders or complete one-file books inside Audiobooks for automatic grouping. Existing contents are never replaced. Folder names remain local and embedded cover artwork is allowed.")}</p>
        </section>
      {:else if activeView === 'Trollbox'}
        <section class="full-panel trollbox-view">
          <div class="panel-title"><span></span><b>{$t("Napstr Trollbox")}</b><span></span></div>
          <div class="trollbox-status"><span><i class:amber={!networkConnected} class="led"></i> {$t("Public Nostr chat:")} <b>#napstr-trollbox</b></span><small>{$t(trollboxLoading ? "Loading…" : "NIP-C7 messages are public and signed by your Napstr Nostr identity.")}</small></div>
          <div class="trollbox-log" use:openChatLog bind:this={trollboxLog} aria-live="polite" aria-busy={trollboxLoading} onscroll={(event) => { if (event.currentTarget.scrollTop < 32) void refreshTrollbox(true); }} aria-label={$t("Napstr public chat messages")}>
            {#if trollboxLoading && !trollboxMessages.length}<p class="trollbox-notice">{$t("Connecting to the trollbox…")}</p>{/if}
            {#if !trollboxLoading && trollboxMessages.length === 0 && !trollboxError}<p class="trollbox-notice">{$t("No messages yet. Say hello.")}</p>{/if}
            {#each trollboxMessages as message (message.eventId)}
              <div class="trollbox-message"><button class="trollbox-name" style:color={chatNameColor(message.npub)} title={$t("Browse songs shared by {p0} · {p1}", { p0: message.displayName, p1: message.npub })} onclick={() => browseUser(message)}>{message.displayName}:</button><span>{message.content}</span>{#if message.npub !== identityNpub}<button class="chat-block" aria-label={$t("Block {p0}", { p0: message.displayName })} onclick={() => blockTrollboxUser(message)}>{$t("Block")}</button>{/if}</div>
            {/each}
          </div>
          {#if trollboxError}<div class="trollbox-error">{$t(trollboxError)}</div>{/if}
          <div class="trollbox-compose">
            <input bind:value={trollboxDraft} maxlength="500" autocomplete="off" placeholder={networkConnected ? $t("Type a public message…") : $t("Connect to Nostr to chat")} disabled={!networkConnected || trollboxSending} aria-label={$t("Trollbox message")} onkeydown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void sendTrollboxMessage(); } }} />
            <button class="classic-button primary" type="button" disabled={!networkConnected || trollboxSending || !trollboxDraft.trim()} onclick={() => void sendTrollboxMessage()}>{trollboxSending ? $t("Sending…") : $t("Send")}</button>
          </div>
        </section>
      {:else if activeView === 'Napstrfy'}
        <section class="full-panel mobile-connect-view">
          <div class="panel-title"><span></span><b>{$t("Napstrfy")}</b><span></span></div>
          <div class="mobile-connect-status">
            <span><i class:amber={!mobileStatusValue?.online} class:error={Boolean(mobileStatusValue?.error)} class="led"></i><b>{mobileStatusValue?.online ? $t("Iroh ready") : mobileStatusValue?.running ? $t("Iroh connecting…") : $t("Iroh unavailable")}</b></span>
            <small>{$t("Napstr stays in control of discovery and Tor downloads.")}</small>
          </div>
          {#if mobileError}<div class="trollbox-error">{$t(mobileError)}</div>{/if}
          <div class="mobile-connect-grid">
            <section class="pair-phone-card">
              <h2>{$t("Pair Napstrfy")}</h2>
              <div class="pairing-tabs" role="tablist" aria-label={$t("Pairing access")}>
                {#each [false, true] as streamOnly}
                  <button type="button" role="tab" id={`pairing-tab-${streamOnly ? 'stream' : 'full'}`} aria-controls={`pairing-panel-${streamOnly ? 'stream' : 'full'}`} aria-selected={mobileStreamOnly === streamOnly} tabindex={mobileStreamOnly === streamOnly ? 0 : -1} onclick={() => (mobileStreamOnly = streamOnly)} onkeydown={navigatePairingTabs}>{streamOnly ? $t("uncle jim") : $t("Full access")}</button>
                {/each}
              </div>
            {#each [false, true] as streamOnly}
              {@const offer = streamOnly ? mobileStreamPairing : mobilePairing}
              <div role="tabpanel" id={`pairing-panel-${streamOnly ? 'stream' : 'full'}`} aria-labelledby={`pairing-tab-${streamOnly ? 'stream' : 'full'}`} hidden={mobileStreamOnly !== streamOnly} tabindex="0">
                <p>{streamOnly ? $t("Read-only, listen to and cache your local music and audiobooks. The connection cannot ask Napstr to download new songs.") : $t("Browse, listen, save songs for offline listening, and ask Napstr to download tracks over Tor.")}</p>
                <p>{$t("Scan in")} <a href="https://napstr.net/napstrfy.html" onclick={openNapstrfyWebsite}>Napstrfy</a>{$t(". Keep Napstr open while streaming.")}</p>
                {#if offer}
                  <div class="pairing-qr" aria-label={streamOnly ? $t("Read-only Napstrfy pairing QR code") : $t("Full-access Napstrfy pairing QR code")}>{@html offer.qrSvg}</div>
                  <p class="pairing-expiry">{$t("One use · expires")} {new Date(offer.expiresAt * 1000).toLocaleTimeString($locale, { hour: '2-digit', minute: '2-digit' })}</p>
                  <details><summary>{$t("Pair without a camera")}</summary><textarea readonly value={offer.ticket} aria-label={streamOnly ? $t("Manual read-only pairing code") : $t("Manual full-access pairing code")}></textarea></details>
                  <button class="classic-button" onclick={() => createMobilePairing(streamOnly)} disabled={mobileLoading}>{mobileLoading ? $t("Preparing…") : $t("Create a new code")}</button>
                {:else}
                  <button class="classic-button primary" onclick={() => createMobilePairing(streamOnly)} disabled={mobileLoading}>{mobileLoading ? $t("Preparing Iroh…") : $t("Create pairing code")}</button>
                  <div class="pairing-placeholder"><span>▦</span><b>{$t("Your one-use QR code will appear here")}</b></div>
                {/if}
              </div>
            {/each}
            </section>
            <section class="paired-devices-card">
              <p>{$t("Napstrfy creates a private, encrypted tunnel from your phone to Napstr, letting you listen to your catalogue by connecting directly to your Napstr instance. Only for your own use and for people you trust.")}</p>
              <h2>{$t("Paired phones")}</h2>
              <p>{$t("Each phone keeps the access granted by its pairing code. Scan a new code to change its access.")}</p>
              <div class="paired-device-list">
                {#each mobileStatusValue?.devices ?? [] as device (device.endpointId)}
                  <div class="paired-device">
                    <span class="phone-glyph">▯</span>
                    <div><b>{device.name}</b><small>{device.streamOnly ? $t("Read only") : $t("Full access")}</small><small>{$t("Last connected")} {mobileLastSeen(device.lastSeen)}</small><code title={device.endpointId}>{device.endpointId}</code></div>
                    <button class="classic-button" onclick={() => revokeMobileDevice(device)}>{$t("Remove")}</button>
                  </div>
                {/each}
                {#if (mobileStatusValue?.devices.length ?? 0) === 0}<p class="empty-state compact">{$t("No phones are paired yet.")}</p>{/if}
              </div>
              <p class="privacy-note wide"><span>i</span> {$t("The QR secret is random, expires after five minutes, and is invalidated by the first successful pairing. Removing a phone immediately revokes future connections.")}</p>
            </section>
          </div>
        </section>
      {:else if activeView === 'Profile'}
        <section class="full-panel profile-view">
          <div class="panel-title"><span></span><b>{$t("Napstr Profile")}</b><span></span></div>
          <div class="profile-card"><div class="avatar"><img src="/napstr-logo.png" alt={$t("Napstr mascot")} /></div><div><h2><button class="user-name" disabled={!identityNpub} onclick={() => browseUser(ownCatalogueUser())}>{displayName}</button></h2><p>{$t("Your dedicated Napstr Nostr identity.")}</p><code>{identityNpub || $t("Connect to create identity")}</code><div class="profile-stats"><span><b>{sharedFiles.length}</b> {$t("shared files")}</span><span><b>{transfers.length}</b> {$t("transfers")}</span><span><b>{networkConnected ? $t("Nostr online") : $t("Offline")}</b></span></div></div></div>
          <fieldset class="edit-profile"><legend>{$t("Profile")}</legend><label>{$t("Display name")} <input bind:value={displayName} /></label><label>{$t("About")} <input bind:value={profileAbout} /></label><label>{$t("Picture URL")} <input bind:value={profilePicture} placeholder="https://…" /></label><button class="classic-button primary" onclick={persistSettings}>{$t("Save profile")}</button></fieldset>
          <p class="privacy-note wide"><span>i</span> {$t("Your profile and shared catalogue are public on Nostr. Transfer addresses and credentials are never published.")}</p>
        </section>
      {:else}
        <section class="full-panel settings-view">
          <div class="panel-title"><span></span><b>{$t("Napstr Settings")}</b><span></span></div>
          <fieldset><legend>{$t("Language")}</legend><LanguageSelect /><small>{$t("Saved automatically on this device.")}</small></fieldset>
          <fieldset><legend>{$t("Network")}</legend><label><input type="checkbox" checked disabled /> {$t("Connect automatically at startup")}</label><label>{$t("Nostr relays")} <input bind:value={nostrRelays} /></label><label>Tor <input value={$t("Bundled, managed automatically")} readonly /></label></fieldset>
          <fieldset><legend>{$t("Files")}</legend><label>{$t("Downloads and shared audio")} <input value={napstrFolder} readonly /><button class="classic-button" onclick={chooseNapstrFolder}>{$t("Browse…")}</button></label><label>{$t("Transfer mode")} <select disabled><option>{$t("Whole file")}</option></select></label><label><input type="checkbox" checked disabled /> {$t("Downloaded audio is automatically shared")}</label><label><input type="checkbox" checked disabled /> {$t("Verify the complete file with SHA-256")}</label></fieldset>
          <div class="settings-actions"><button class="classic-button primary" onclick={persistSettings}>{$t("OK")}</button><button class="classic-button" onclick={refreshSnapshot}>{$t("Cancel")}</button><button class="classic-button" onclick={persistSettings}>{$t("Apply")}</button></div>
        </section>
      {/if}
    </div>

    <section class="transfer-dock">
      <button
        type="button"
        class="dock-resizer"
        aria-label={$t("Resize Transfer Manager")}
        title={$t("Drag to resize Transfer Manager · double-click to reset")}
        onpointerdown={beginTransferResize}
        onkeydown={resizeTransferWithKeyboard}
        ondblclick={() => setTransferPaneHeight(window.innerHeight < 700 ? 94 : 119, true)}
      ></button>
      <div class="dock-title"><span></span><b>{$t("Transfer Manager")}</b><span></span><button class="dock-clear" onclick={clearFinishedTransfers} disabled={clearingTransfers || !transfers.some(isFinishedTransfer)}>{$t("Clear finished")}</button><button class="dock-clear" onclick={clearAllTransfers} disabled={clearingTransfers || removingTransfers.size > 0 || (!transfers.length && !audiobookDownloads.length && !startingDownloads.size)}>{clearingTransfers ? $t("Clearing…") : $t("Clear all")}</button><button onclick={() => (activeView = 'Downloads')} title={$t("Open Download Manager")}>□</button></div>
      <div class="mini-transfers">
        {#each audiobookDownloads as book}
          <div class="mini-row audiobook-mini-row"><span class="audiobook-glyph">▥</span><span class="mini-name">{book.title} {$t("· chapter")} {Math.min(book.nextIndex + 1, book.chapters.length)} {$t("of")} {book.chapters.length}</span><div class="progress"><span style={`width:${book.chapters.length ? (book.nextIndex / book.chapters.length) * 100 : 0}%`}></span></div><span class="mini-size">{readableSize(book.chapters.reduce((sum, chapter) => sum + chapter.size, 0))}</span><span class="mini-status">{$t("Book")}</span></div>
        {/each}
        {#each transfers as transfer}
          <div class:transfer-complete={isCompleteTransfer(transfer)} class="mini-row">{#if isCompleteTransfer(transfer)}<button class="mini-play" onclick={() => playAudio(transfer.fileId, transfer.name, playerMode, 'downloads')} title={$t("Play verified audio")}>▶</button>{:else}<span class="download-arrow">⇩</span>{/if}<span class="mini-name">{transfer.name}</span><div class="progress"><span style={`width:${transfer.progress}%`}></span></div><span class="mini-size">{transfer.size}</span><span class="mini-status" title={isCompleteTransfer(transfer) ? $t("Ready") : transfer.status}>{isCompleteTransfer(transfer) ? $t("Ready") : transfer.speed}</span><button class="tiny-button mini-cancel" onclick={() => removeTransfer(transfer.id)} disabled={clearingTransfers || removingTransfers.has(transfer.id)} aria-label={`${isActiveTransfer(transfer) ? 'Cancel download' : 'Clear entry'}: ${transfer.name}`} title={isActiveTransfer(transfer) ? $t("Cancel download and remove partial file") : $t("Clear entry; keep completed audio")}>×</button></div>
        {/each}
      </div>
    </section>

    <footer class="statusbar"><span>{$t(activityMessage)}</span><span><i class:amber={!networkConnected} class="led"></i> Nostr {networkConnected ? $t("online") : $t("offline")}</span><span title={torError}>{$t("♜ Tor:")} {torRunning ? $t("ready") : torError ? $t("failed") : torStarting && torProgress > 0 ? `${torProgress}%` : $t("starting")}</span><span class="status-clock">{new Intl.DateTimeFormat($locale, { hour: '2-digit', minute: '2-digit' }).format(clockNow)}</span></footer>
  </section>

  {#if aboutOpen}
    <div class="modal-backdrop" role="presentation" onclick={() => (aboutOpen = false)}>
      <dialog class="dialog" open aria-label={$t("About Napstr")} onclick={(e) => e.stopPropagation()} onkeydown={(e) => { if (e.key === 'Escape') aboutOpen = false; }}>
        <header class="titlebar"><div class="title-left"><span class="app-icon"><img src="/napstr-logo.png" alt="" /></span><span>{$t("About Napstr")}</span></div><div class="window-controls"><button onclick={() => (aboutOpen = false)}>×</button></div></header>
        <div class="dialog-body about-dialog-body">
          <div class="about-summary"><div class="about-logo"><img src="/napstr-logo.png" alt="" /></div><div><h2>Napstr</h2><p>{$t("Version")} {appVersion}</p><p>{$t("Public discovery over Nostr.")}<br />{$t("Private verified transfers over Tor.")}</p></div></div>
          <p class="about-donation">{$t("donations welcome!")}<br /><code>bc1qwgms685z3j69qtgalyjtrfuqg5f6pt302z0k60</code></p>
        </div>
        <div class="dialog-actions"><button class="classic-button primary" onclick={() => (aboutOpen = false)}>{$t("OK")}</button></div>
      </dialog>
    </div>
  {/if}

  {#if sourceProfile}
    <div class="modal-backdrop" role="presentation" onclick={() => (sourceProfile = null)}>
      <dialog class="dialog" open aria-label={$t("Napstr public profile")} onclick={(e) => e.stopPropagation()}>
        <header class="titlebar"><div class="title-left"><span class="app-icon"><img src="/napstr-logo.png" alt="" /></span><span>{$t("Public Napstr Profile")}</span></div><div class="window-controls"><button onclick={() => (sourceProfile = null)}>×</button></div></header>
        <div class="dialog-body"><div class="about-logo">☺</div><div><h2><button class="user-name" onclick={() => browseUser(sourceProfile!)}>{sourceProfile.displayName}</button></h2><p>{sourceProfile.about || $t("No profile description published.")}</p><code>{sourceProfile.npub}</code></div></div>
        <div class="dialog-actions"><button class="classic-button primary" onclick={() => (sourceProfile = null)}>{$t("OK")}</button></div>
      </dialog>
    </div>
  {/if}

  {#if audiobookEditorOpen}
    <div class="modal-backdrop" role="presentation" onclick={() => { if (!audiobookSaving) audiobookEditorOpen = false; }}>
      <dialog class="dialog audiobook-dialog" open aria-label={$t("Group folder as audiobook")} onclick={(event) => event.stopPropagation()} onkeydown={(event) => { if (event.key === 'Escape' && !audiobookSaving) audiobookEditorOpen = false; }}>
        <header class="titlebar"><div class="title-left"><span class="app-icon">▥</span><span>{$t("Publish Audiobook")}</span></div><div class="window-controls"><button disabled={audiobookSaving} onclick={() => (audiobookEditorOpen = false)}>×</button></div></header>
        <div class="audiobook-dialog-body">
          <p>{$t("Napstr will publish this folder as one ordered audiobook while retaining its normal chapter file events.")}</p>
          <label>{$t("Title")} <input bind:value={audiobookTitle} maxlength="256" /></label>
          <label>{$t("Author")} <input bind:value={audiobookAuthor} maxlength="256" /></label>
          <label>{$t("Narrator")} <input bind:value={audiobookNarrator} maxlength="256" /></label>
          <fieldset><legend>{$t("Chapter order")}</legend><div class="audiobook-preview">{#each audiobookFolderFiles() as file, index}<div><span>{String(index + 1).padStart(2, '0')}</span><b>{file.title || file.filename}</b><small>{file.readableSize}</small></div>{/each}</div></fieldset>
          <p class="privacy-note"><span>i</span> {$t("The title, author, narrator, chapter names, and ordered file hashes will be public. Your folder name and filesystem path remain private.")}</p>
        </div>
        <div class="dialog-actions audiobook-dialog-actions">{#if currentFolderAudiobook()}<button class="classic-button" disabled={audiobookSaving} onclick={ungroupAudiobook}>{$t("Publish separately")}</button>{/if}<span></span><button class="classic-button primary" disabled={audiobookSaving || !audiobookTitle.trim()} onclick={saveAudiobookGroup}>{audiobookSaving ? $t("Publishing…") : $t("Save & publish")}</button><button class="classic-button" disabled={audiobookSaving} onclick={() => (audiobookEditorOpen = false)}>{$t("Cancel")}</button></div>
      </dialog>
    </div>
  {/if}

  {#if blockConfirmation}
    <div class="modal-backdrop" role="presentation" onclick={() => { if (!blockInProgress) blockConfirmation = null; }}>
      <dialog class="dialog confirm-dialog" open aria-label={$t("Confirm block")} onclick={(e) => e.stopPropagation()} onkeydown={(e) => { if (e.key === 'Escape' && !blockInProgress) blockConfirmation = null; }}>
        <header class="titlebar"><div class="title-left"><span class="app-icon">!</span><span>{$t("Confirm block")}</span></div><div class="window-controls"><button disabled={blockInProgress} onclick={() => (blockConfirmation = null)}>×</button></div></header>
        <div class="dialog-body"><div class="confirm-icon">!</div><div><h3>{$t("Are you sure?")}</h3>{#if blockConfirmation.kind === 'file'}<p>{$t("Block")} <strong>{blockConfirmation.label}</strong>?</p><p>{$t("Every seeder offering these exact file bytes will be hidden.")}</p>{:else}<p>{$t("Block")} <strong>{blockConfirmation.label}</strong>?</p><p>{$t("Their catalogue entries, public chat messages, and download requests will be ignored.")}</p>{/if}</div></div>
        <div class="dialog-actions"><button class="classic-button primary" disabled={blockInProgress} onclick={confirmBlock}>{blockInProgress ? $t("Blocking…") : $t("Block")}</button><button class="classic-button" disabled={blockInProgress} onclick={() => (blockConfirmation = null)}>{$t("Cancel")}</button></div>
      </dialog>
    </div>
  {/if}
</main>
{/if}
