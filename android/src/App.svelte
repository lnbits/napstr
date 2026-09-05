<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import {
    Format,
    checkPermissions,
    openAppSettings,
    requestPermissions,
    scan
  } from '@tauri-apps/plugin-barcode-scanner';
  import TrackArtwork from './lib/TrackArtwork.svelte';
  import type { AudiobookLibraryPage, CachedAudio, CompanionStatus, LibraryPage, PodcastDownload, PodcastEpisode, PodcastFeed, RemoteAudiobook, RemoteAudiobookSummary, RemoteTrack, RemoteTransfer } from './lib/types';

  const musicChips = ['Rock', 'Soundtrack', 'Punk', 'Folk', 'Upbeat'];
  const podcastGenres = ['Comedy', 'News', 'True Crime', 'Society & Culture', 'Technology', 'History', 'Business', 'Science', 'Arts', 'Sports', 'Education', 'Music'];
  const likedMusicKey = 'napstrfy-liked-music';
  const likedPodcastsKey = 'napstrfy-liked-podcasts';
  type AppTab = 'music' | 'podcasts' | 'audiobooks';
  type PlayMode = 'all' | 'random' | 'repeat' | 'once';
  const playModes: Array<{ value: PlayMode; icon: string; label: string }> = [
    { value: 'all', icon: '↻A', label: 'Play all' },
    { value: 'random', icon: '⤨', label: 'Play random' },
    { value: 'repeat', icon: '↻1', label: 'Repeat track' },
    { value: 'once', icon: '▶1', label: 'Play once' }
  ];
  let activeTab = $state<AppTab>('music');
  let status = $state<CompanionStatus>({ paired: false, connected: false, desktopName: '', endpointId: '', libraryRevision: 0, error: '' });
  let statusLoading = $state(true);
  let statusPending = $state(false);
  let pairingCode = $state('');
  let pairing = $state(false);
  let scanning = $state(false);
  let cameraPermissionDenied = $state(false);
  let error = $state('');
  let notice = $state('');
  let query = $state('');
  let tracks = $state<RemoteTrack[]>([]);
  let likedMusic = $state<RemoteTrack[]>([]);
  let showingLikedMusic = $state(false);
  let total = $state(0);
  let loading = $state(false);
  let loadingMore = $state(false);
  let musicViewVersion = 0;
  let loadedLibraryRevision = 0;
  let silentLibraryRefresh = false;
  let cacheReconciliationKey = '';
  let cacheReconciliationPending = false;
  let selected = $state<RemoteTrack | null>(null);
  let current = $state<RemoteTrack | null>(null);
  let playerQueue = $state<RemoteTrack[]>([]);
  let playerQueueLibraryVisible = true;
  let playerIndex = $state(-1);
  let playMode = $state<PlayMode>('all');
  let randomHistory = $state<number[]>([]);
  let randomHistoryIndex = $state(-1);
  let randomUpcoming = $state(-1);
  let playing = $state(false);
  let caching = $state(false);
  let currentTime = $state(0);
  let duration = $state(0);
  let volume = $state(0.85);
  let pending = $state(new Map<string, string>());
  let pendingAudiobooks = $state(new Map<string, string>());
  let transfers = $state<RemoteTransfer[]>([]);
  let audiobookQuery = $state('');
  let audiobooks = $state<RemoteAudiobookSummary[]>([]);
  let audiobookTotal = $state(0);
  let selectedAudiobook = $state<RemoteAudiobook | null>(null);
  let audiobookLoading = $state(false);
  let podcastQuery = $state('');
  let podcastFeeds = $state<PodcastFeed[]>([]);
  let likedPodcasts = $state<PodcastFeed[]>([]);
  let showingLikedPodcasts = $state(false);
  let podcastGenre = $state('');
  let selectedPodcast = $state<PodcastFeed | null>(null);
  let podcastEpisodes = $state<PodcastEpisode[]>([]);
  let podcastHistory = $state<PodcastEpisode[]>([]);
  let podcastDownloads = $state<PodcastDownload[]>([]);
  let podcastLoading = $state(false);
  let podcastViewVersion = 0;
  let currentPodcast = $state<PodcastEpisode | null>(null);
  let activeMedia = $state<'music' | 'podcast'>('music');
  let audio: HTMLAudioElement;
  let lastSystemMediaSync = 0;

  type AndroidMediaBridge = {
    update(payload: string): void;
    clear(): void;
  };

  function androidMediaBridge(): AndroidMediaBridge | undefined {
    return (window as Window & { NapstrfyMedia?: AndroidMediaBridge }).NapstrfyMedia;
  }

  function title(track: RemoteTrack) {
    return track.title || track.filename;
  }

  function artist(track: RemoteTrack) {
    return track.artist || 'Unknown artist';
  }

  function isStoredTrack(value: unknown): value is RemoteTrack {
    if (!value || typeof value !== 'object') return false;
    const item = value as Partial<RemoteTrack>;
    return typeof item.fileId === 'string' && item.fileId.length <= 128 &&
      typeof item.filename === 'string' && item.filename.length <= 500 &&
      typeof item.title === 'string' && typeof item.artist === 'string' &&
      typeof item.album === 'string' && typeof item.format === 'string' &&
      typeof item.mime === 'string' && typeof item.size === 'number' &&
      typeof item.tags === 'string' && typeof item.local === 'boolean' &&
      Array.isArray(item.sources);
  }

  function isStoredPodcast(value: unknown): value is PodcastFeed {
    if (!value || typeof value !== 'object') return false;
    const item = value as Partial<PodcastFeed>;
    return typeof item.id === 'number' && Number.isFinite(item.id) &&
      typeof item.title === 'string' && item.title.length <= 500 &&
      typeof item.author === 'string' && typeof item.description === 'string' &&
      typeof item.feedUrl === 'string' && typeof item.image === 'string' &&
      typeof item.language === 'string' && typeof item.episodeCount === 'number';
  }

  function saveLikes(key: string, value: unknown) {
    try {
      window.localStorage.setItem(key, JSON.stringify(value));
    } catch {
      error = 'Napstrfy could not save that favourite on this phone.';
    }
  }

  function isTrackLiked(track: RemoteTrack) {
    return likedMusic.some((item) => item.fileId === track.fileId);
  }

  function toggleTrackLike(track: RemoteTrack) {
    likedMusic = isTrackLiked(track)
      ? likedMusic.filter((item) => item.fileId !== track.fileId)
      : [track, ...likedMusic.filter((item) => item.fileId !== track.fileId)].slice(0, 1000);
    saveLikes(likedMusicKey, likedMusic);
    if (showingLikedMusic) {
      tracks = [...likedMusic];
      total = tracks.length;
      if (!tracks.some((item) => item.fileId === selected?.fileId)) selected = tracks[0] ?? null;
    }
  }

  function isPodcastLiked(feed: PodcastFeed) {
    return likedPodcasts.some((item) => item.id === feed.id);
  }

  function togglePodcastLike(feed: PodcastFeed) {
    likedPodcasts = isPodcastLiked(feed)
      ? likedPodcasts.filter((item) => item.id !== feed.id)
      : [feed, ...likedPodcasts.filter((item) => item.id !== feed.id)].slice(0, 500);
    saveLikes(likedPodcastsKey, likedPodcasts);
    if (showingLikedPodcasts) podcastFeeds = [...likedPodcasts];
  }

  function showLikedTracks() {
    musicViewVersion += 1;
    showingLikedMusic = !showingLikedMusic;
    if (!showingLikedMusic) {
      void searchTracks(query);
      return;
    }
    loading = false;
    loadingMore = false;
    tracks = [...likedMusic];
    total = tracks.length;
    selected = tracks[0] ?? null;
  }

  function playModeDetails() {
    return playModes.find((mode) => mode.value === playMode) ?? playModes[0];
  }

  function randomIndexExcept(currentIndex: number) {
    if (playerQueue.length < 2) return -1;
    const candidate = Math.floor(Math.random() * (playerQueue.length - 1));
    return candidate >= currentIndex ? candidate + 1 : candidate;
  }

  function resetRandomOrder() {
    randomHistory = playerIndex >= 0 ? [playerIndex] : [];
    randomHistoryIndex = randomHistory.length - 1;
    randomUpcoming = randomIndexExcept(playerIndex);
  }

  function cyclePlayMode() {
    const index = playModes.findIndex((mode) => mode.value === playMode);
    playMode = playModes[(index + 1) % playModes.length].value;
    window.localStorage.setItem('napstrfy-play-mode', playMode);
    if (playMode === 'random') resetRandomOrder();
    syncSystemMedia(true);
  }

  function readableSize(size: number) {
    if (size < 1024 * 1024) return `${Math.max(1, Math.round(size / 1024))} KB`;
    return `${(size / 1024 / 1024).toFixed(size >= 10 * 1024 * 1024 ? 0 : 1)} MB`;
  }

  function clock(seconds: number) {
    if (!Number.isFinite(seconds)) return '0:00';
    const whole = Math.max(0, Math.floor(seconds));
    return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`;
  }

  async function refreshStatus(showError = false, syncLibrary = true) {
    if (statusPending) return;
    statusPending = true;
    try {
      const wasConnected = status.connected;
      status = await invoke<CompanionStatus>('companion_status');
      if (showError && status.error) error = status.error;
      if (status.connected) {
        void reconcileAudioCache();
        if (syncLibrary && (!wasConnected || (status.libraryRevision > 0
          && loadedLibraryRevision > 0 && status.libraryRevision !== loadedLibraryRevision))) {
          void refreshLibrarySilently(status.libraryRevision);
        }
      }
    } catch (nextError) {
      if (showError) error = String(nextError);
    } finally {
      statusLoading = false;
      statusPending = false;
    }
  }

  async function loadCachedLibrary() {
    try {
      const offline = await invoke<LibraryPage & { paired: boolean; desktopName: string }>('cached_library');
      if (offline.paired) {
        status = { ...status, paired: true, desktopName: offline.desktopName };
      }
      tracks = offline.tracks;
      total = offline.total;
      if (!selected || !tracks.some((track) => track.fileId === selected?.fileId)) selected = tracks[0] ?? null;
    } catch {
      // A damaged cache must never prevent pairing or normal online use.
    }
  }

  async function reconcileAudioCache() {
    if (!status.connected || cacheReconciliationPending) return;
    const key = `${status.endpointId}:${status.libraryRevision}`;
    if (cacheReconciliationKey === key) return;
    cacheReconciliationPending = true;
    try {
      const complete = await invoke<boolean>('reconcile_audio_cache', {
        protectedFileIds: playing && activeMedia === 'music' && current ? [current.fileId] : []
      });
      if (complete) cacheReconciliationKey = key;
    } catch (nextError) {
      // Older Napstr versions do not implement cache reconciliation. Preserve
      // every offline file and avoid repeatedly asking during this session.
      if (/invalid Napstrfy request|unexpected response/i.test(String(nextError))) {
        cacheReconciliationKey = key;
      }
    } finally {
      cacheReconciliationPending = false;
    }
  }

  async function reconnect() {
    await refreshStatus(true, false);
    if (status.connected) await loadLibrary();
  }

  async function pair(code = pairingCode) {
    if (!code.trim() || pairing) return;
    pairing = true;
    error = '';
    try {
      const platform = /iPhone|iPad|iPod/i.test(navigator.userAgent) ? 'iPhone' : 'Android phone';
      const desktop = await invoke<string>('pair_desktop', { code: code.trim(), deviceName: `Napstrfy on ${platform}` });
      pairingCode = '';
      notice = `Connected to ${desktop}`;
      await refreshStatus();
      await loadLibrary();
    } catch (nextError) {
      error = String(nextError);
    } finally {
      pairing = false;
    }
  }

  async function scanCode() {
    if (scanning || pairing) return;
    error = '';
    cameraPermissionDenied = false;
    scanning = true;
    try {
      let permission = await checkPermissions();
      if (permission !== 'granted') permission = await requestPermissions();
      if (permission !== 'granted') {
        cameraPermissionDenied = true;
        error = 'Camera access is required to scan the Napstr pairing code.';
        return;
      }

      const result = await scan({
        cameraDirection: 'back',
        formats: [Format.QRCode],
        windowed: false
      });
      pairingCode = result.content;
      await pair(result.content);
    } catch (nextError) {
      const message = String(nextError);
      if (!/cancel/i.test(message)) {
        cameraPermissionDenied = /permission/i.test(message);
        error = cameraPermissionDenied
          ? 'Camera access is required to scan the Napstr pairing code.'
          : `Could not open the QR scanner: ${message}`;
      }
    } finally {
      scanning = false;
    }
  }

  async function showCameraSettings() {
    try {
      await openAppSettings();
    } catch (nextError) {
      error = `Could not open Android settings: ${String(nextError)}`;
    }
  }

  async function forgetDesktop() {
    if (!window.confirm('Disconnect this phone from Napstr? You will need to scan a new QR code.')) return;
    await invoke('forget_desktop');
    status = { paired: false, connected: false, desktopName: '', endpointId: '', libraryRevision: 0, error: '' };
    tracks = [];
    current = null;
    audio?.pause();
  }

  async function loadLibrary(append = false) {
    if (!status.paired || loading || loadingMore) return;
    const viewVersion = ++musicViewVersion;
    showingLikedMusic = false;
    append ? (loadingMore = true) : (loading = true);
    error = '';
    try {
      const page = await invoke<LibraryPage>('remote_library', {
        query: query.trim(),
        offset: append ? tracks.length : 0,
        limit: 100
      });
      if (viewVersion !== musicViewVersion) return;
      tracks = append ? [...tracks, ...page.tracks] : page.tracks;
      total = page.total;
      loadedLibraryRevision = status.libraryRevision;
      if (!selected || !tracks.some((track) => track.fileId === selected?.fileId)) selected = tracks[0] ?? null;
    } catch (nextError) {
      if (viewVersion === musicViewVersion) error = String(nextError);
    } finally {
      if (viewVersion === musicViewVersion) {
        loading = false;
        loadingMore = false;
      }
    }
  }

  async function refreshLibrarySilently(revision: number) {
    if (silentLibraryRefresh || loading || loadingMore || !status.connected) return;
    if (showingLikedMusic || query.trim()) {
      // These views issue a fresh request when the user opens or submits them.
      loadedLibraryRevision = revision;
      return;
    }
    silentLibraryRefresh = true;
    try {
      const page = await invoke<LibraryPage>('remote_library', { query: '', offset: 0, limit: 100 });
      tracks = page.tracks;
      total = page.total;
      loadedLibraryRevision = revision;
      if (!selected || !tracks.some((track) => track.fileId === selected?.fileId)) selected = tracks[0] ?? null;
    } catch {
      // Keep the current list visible and retry after the next status check.
    } finally {
      silentLibraryRefresh = false;
    }
  }

  async function searchTracks(nextQuery = query) {
    query = nextQuery;
    showingLikedMusic = false;
    if (!query.trim()) return loadLibrary();
    if (loading) return;
    const viewVersion = ++musicViewVersion;
    loading = true;
    error = '';
    try {
      const results = await invoke<RemoteTrack[]>('remote_search', { query: query.trim() });
      if (viewVersion !== musicViewVersion) return;
      tracks = results;
      total = tracks.length;
      selected = tracks[0] ?? null;
    } catch (nextError) {
      if (viewVersion === musicViewVersion) error = String(nextError);
    } finally {
      if (viewVersion === musicViewVersion) loading = false;
    }
  }

  async function showAudiobooks() {
    activeTab = 'audiobooks';
    if (audiobooks.length === 0) await loadAudiobooks();
  }

  async function loadAudiobooks() {
    if (!status.connected || audiobookLoading) return;
    audiobookLoading = true;
    selectedAudiobook = null;
    error = '';
    try {
      const page = await invoke<AudiobookLibraryPage>('remote_audiobook_library', {
        query: audiobookQuery.trim(), offset: 0, limit: 100
      });
      audiobooks = page.audiobooks;
      audiobookTotal = page.total;
    } catch (nextError) {
      error = String(nextError);
    } finally {
      audiobookLoading = false;
    }
  }

  async function openAudiobook(book: RemoteAudiobookSummary) {
    if (audiobookLoading) return;
    audiobookLoading = true;
    error = '';
    try {
      selectedAudiobook = await invoke<RemoteAudiobook>('remote_audiobook', { audiobookId: book.audiobookId });
    } catch (nextError) {
      error = String(nextError);
    } finally {
      audiobookLoading = false;
    }
  }

  async function activateAudiobookChapter(book: RemoteAudiobook, track: RemoteTrack) {
    activeMedia = 'music';
    selected = track;
    if (!track.local) {
      await requestDownload(track, audiobookDestinationFolder(book), book.audiobookId);
      return;
    }
    playerQueue = book.chapters.filter((chapter) => chapter.local);
    playerQueueLibraryVisible = false;
    playerIndex = playerQueue.findIndex((chapter) => chapter.fileId === track.fileId);
    resetRandomOrder();
    await playTrack(track);
  }

  async function activateTrack(track: RemoteTrack) {
    activeMedia = 'music';
    selected = track;
    if (!track.local) {
      await requestDownload(track);
      return;
    }
    const queue = tracks.filter((item) => item.local);
    playerQueue = queue;
    playerQueueLibraryVisible = true;
    playerIndex = queue.findIndex((item) => item.fileId === track.fileId);
    resetRandomOrder();
    await playTrack(track);
  }

  async function playTrack(track: RemoteTrack, libraryVisible = playerQueueLibraryVisible) {
    if (caching) return;
    caching = true;
    error = '';
    current = track;
    activeMedia = 'music';
    try {
      audio?.pause();
      const cached = await invoke<CachedAudio>('cache_remote_audio', { track, libraryVisible });
      current = cached.track;
      await tick();
      audio.src = cached.url;
      audio.volume = volume;
      await audio.play();
      playing = true;
      const nextIndex = playMode === 'random'
        ? randomUpcoming
        : playerQueue.length > 1
          ? (playerIndex + 1) % playerQueue.length
          : -1;
      const next = nextIndex >= 0 ? playerQueue[nextIndex] : undefined;
      if (next?.local) {
        void invoke('prefetch_remote_audio', {
          afterFileId: cached.track.fileId,
          track: next,
          libraryVisible
        });
      }
    } catch (nextError) {
      playing = false;
      error = `Could not play ${title(track)}: ${String(nextError)}`;
    } finally {
      caching = false;
    }
  }

  function audiobookDestinationFolder(book: RemoteAudiobook) {
    const title = book.title
      .replace(/[\x00-\x1f/\\:*?"<>|]/g, '_')
      .replace(/^[.\s]+|[.\s]+$/g, '')
      .slice(0, 86) || 'Audiobook';
    return `${title} [${book.audiobookId.slice(0, 8)}]`;
  }

  async function requestDownload(
    track: RemoteTrack,
    destinationFolder: string | null = null,
    audiobookId: string | null = null
  ) {
    if (pending.has(track.fileId)) return;
    pending = new Map(pending).set(track.fileId, track.filename);
    if (audiobookId) pendingAudiobooks = new Map(pendingAudiobooks).set(track.fileId, audiobookId);
    error = '';
    try {
      await invoke<string>('remote_download', {
        fileId: track.fileId,
        sourcePubkeys: track.sources.map((source) => source.pubkey),
        destinationFolder
      });
      notice = `Napstr is downloading ${title(track)} over Tor`;
      await refreshTransfers();
    } catch (nextError) {
      const next = new Map(pending);
      next.delete(track.fileId);
      pending = next;
      const nextAudiobooks = new Map(pendingAudiobooks);
      nextAudiobooks.delete(track.fileId);
      pendingAudiobooks = nextAudiobooks;
      error = String(nextError);
    }
  }

  async function refreshTransfers() {
    if (!status.connected || pending.size === 0) return;
    try {
      transfers = await invoke<RemoteTransfer[]>('remote_transfers');
      for (const fileId of [...pending]) {
        const [pendingFileId, pendingFilename] = fileId;
        const transfer = transfers.find((item) => item.fileId === pendingFileId);
        if (transfer && /failed|cancel/i.test(transfer.status)) {
          const next = new Map(pending);
          next.delete(pendingFileId);
          pending = next;
          const nextAudiobooks = new Map(pendingAudiobooks);
          nextAudiobooks.delete(pendingFileId);
          pendingAudiobooks = nextAudiobooks;
          error = `${transfer.filename}: ${transfer.status}`;
          continue;
        }
        if (transfer && transfer.progress < 100 && !/complete|verified/i.test(transfer.status)) continue;
        const original = tracks.find((item) => item.fileId === pendingFileId)
          ?? selectedAudiobook?.chapters.find((item) => item.fileId === pendingFileId);
        const audiobookId = pendingAudiobooks.get(pendingFileId);
        let local: RemoteTrack | undefined;
        if (audiobookId) {
          const refreshed = await invoke<RemoteAudiobook>('remote_audiobook', { audiobookId });
          local = refreshed.chapters.find((item) => item.fileId === pendingFileId && item.local);
          if (selectedAudiobook?.audiobookId === audiobookId) selectedAudiobook = refreshed;
        } else {
          const page = await invoke<LibraryPage>('remote_library', { query: original?.filename || transfer?.filename || pendingFilename, offset: 0, limit: 20 });
          local = page.tracks.find((item) => item.fileId === pendingFileId);
        }
        if (!local) continue;
        tracks = tracks.map((item) => item.fileId === pendingFileId ? local : item);
        if (selectedAudiobook) selectedAudiobook = {
          ...selectedAudiobook,
          chapters: selectedAudiobook.chapters.map((chapter) => chapter.fileId === pendingFileId ? local : chapter)
        };
        if (likedMusic.some((item) => item.fileId === pendingFileId)) {
          likedMusic = likedMusic.map((item) => item.fileId === pendingFileId ? local : item);
          saveLikes(likedMusicKey, likedMusic);
        }
        if (selected?.fileId === pendingFileId) selected = local;
        const next = new Map(pending);
        next.delete(pendingFileId);
        pending = next;
        const nextAudiobooks = new Map(pendingAudiobooks);
        nextAudiobooks.delete(pendingFileId);
        pendingAudiobooks = nextAudiobooks;
        notice = `${title(local)} is ready to play`;
      }
    } catch { /* the next foreground poll retries */ }
  }

  function togglePlayer() {
    if (!current && !currentPodcast) return;
    if (audio.paused) audio.play().catch((nextError) => (error = String(nextError)));
    else audio.pause();
  }

  function syncSystemMedia(force = false) {
    const bridge = androidMediaBridge();
    const media = activeMedia === 'podcast' ? currentPodcast : current;
    if (!bridge || !media) return;
    const now = performance.now();
    if (!force && now - lastSystemMediaSync < 900) return;
    lastSystemMediaSync = now;
    bridge.update(JSON.stringify({
      title: activeMedia === 'podcast' ? currentPodcast?.title : current ? title(current) : '',
      artist: activeMedia === 'podcast' ? currentPodcast?.feedTitle : current ? artist(current) : '',
      playing,
      position: Number.isFinite(currentTime) ? currentTime : 0,
      duration: Number.isFinite(duration) ? duration : 0,
      canPrevious: activeMedia === 'music' && playerQueue.length > 1 && (playMode !== 'random' || randomHistoryIndex > 0),
      canNext: activeMedia === 'music' && playerQueue.length > 1
    }));
  }

  function handleSystemMediaAction(event: Event) {
    const action = (event as CustomEvent<string>).detail;
    if (!audio) return;
    if (action === 'play') {
      if (audio.paused) audio.play().catch((nextError) => (error = String(nextError)));
    } else if (action === 'pause') {
      if (!audio.paused) audio.pause();
    } else if (action === 'previous') {
      void moveTrack(-1);
    } else if (action === 'next') {
      void moveTrack(1);
    } else if (action.startsWith('seek:')) {
      const milliseconds = Number(action.slice(5));
      if (Number.isFinite(milliseconds)) seek(milliseconds / 1000);
    }
  }

  function seek(value: number) {
    if (!audio || !Number.isFinite(audio.duration)) return;
    audio.currentTime = value;
    currentTime = value;
  }

  function setVolume(value: number) {
    volume = value;
    if (audio) audio.volume = value;
  }

  async function moveTrack(direction: -1 | 1) {
    if (activeMedia !== 'music' || playerQueue.length < 2) return;
    let next: number;
    if (playMode === 'random') {
      if (direction === -1) {
        if (randomHistoryIndex <= 0) return;
        randomHistoryIndex -= 1;
        next = randomHistory[randomHistoryIndex];
        randomUpcoming = randomHistory[randomHistoryIndex + 1] ?? randomIndexExcept(next);
      } else if (randomHistoryIndex + 1 < randomHistory.length) {
        randomHistoryIndex += 1;
        next = randomHistory[randomHistoryIndex];
        randomUpcoming = randomHistory[randomHistoryIndex + 1] ?? randomIndexExcept(next);
      } else {
        next = randomUpcoming >= 0 ? randomUpcoming : randomIndexExcept(playerIndex);
        if (next < 0) return;
        randomHistory = [...randomHistory.slice(0, randomHistoryIndex + 1), next].slice(-100);
        randomHistoryIndex = randomHistory.length - 1;
        randomUpcoming = randomIndexExcept(next);
      }
    } else {
      next = (playerIndex + direction + playerQueue.length) % playerQueue.length;
    }
    playerIndex = next;
    selected = playerQueue[next];
    await playTrack(playerQueue[next]);
  }

  function handleTrackEnded() {
    playing = false;
    syncSystemMedia(true);
    if (activeMedia !== 'music') return;
    if (playMode === 'once') return;
    if (playMode === 'repeat') {
      audio.currentTime = 0;
      audio.play().catch((nextError) => (error = String(nextError)));
      return;
    }
    void moveTrack(1);
  }

  function selectChip(chip: string) {
    void searchTracks(query.toLocaleLowerCase() === chip.toLocaleLowerCase() ? '' : chip);
  }

  function normalizeGenre(value: string) {
    return value.toLocaleLowerCase().replace(/[^\p{L}\p{N}]+/gu, ' ').trim();
  }

  function feedMatchesGenre(feed: PodcastFeed, genre: string) {
    const wanted = normalizeGenre(genre);
    return feed.genres.some((value) => {
      const candidate = normalizeGenre(value);
      return candidate === wanted || candidate.includes(wanted) || wanted.includes(candidate);
    });
  }

  function podcastDate(timestamp: number) {
    if (!timestamp) return '';
    return new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short', year: 'numeric' })
      .format(new Date(timestamp * 1000));
  }

  function podcastDownloadFor(episodeId: number) {
    return podcastDownloads.find((download) => download.episode.id === episodeId);
  }

  async function invokePodcast<T>(command: string, args?: Record<string, unknown>): Promise<T> {
    let timeout = 0;
    try {
      return await Promise.race([
        invoke<T>(command, args),
        new Promise<T>((_, reject) => {
          timeout = window.setTimeout(
            () => reject(new Error('The podcast service did not respond. Check your connection and retry.')),
            20_000
          );
        })
      ]);
    } finally {
      window.clearTimeout(timeout);
    }
  }

  async function fetchPodcastDirectory(
    path: 'search' | 'lookup',
    parameters: Record<string, string>,
    maximumBytes: number
  ): Promise<string> {
    const url = new URL(`https://itunes.apple.com/${path}`);
    for (const [name, value] of Object.entries(parameters)) url.searchParams.set(name, value);
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 15_000);
    try {
      const response = await fetch(url, {
        headers: { Accept: 'application/json' },
        signal: controller.signal
      });
      if (!response.ok) throw new Error(`Podcast directory returned ${response.status}.`);
      const advertisedLength = Number(response.headers.get('content-length') || 0);
      if (advertisedLength > maximumBytes) throw new Error('Podcast directory response is too large.');
      const payload = await response.text();
      if (new TextEncoder().encode(payload).byteLength > maximumBytes) {
        throw new Error('Podcast directory response is too large.');
      }
      return payload;
    } catch (nextError) {
      if (controller.signal.aborted) {
        throw new Error('The podcast directory did not respond. Check your connection and retry.');
      }
      throw nextError;
    } finally {
      window.clearTimeout(timeout);
    }
  }

  async function searchPodcastDirectory(searchTerm: string, limit: number): Promise<PodcastFeed[]> {
    const query = searchTerm.trim();
    if (!query || query.length > 120) throw new Error('Search for between 1 and 120 characters.');
    const boundedLimit = Math.min(50, Math.max(1, limit));
    const payload = await fetchPodcastDirectory('search', {
      term: query,
      media: 'podcast',
      entity: 'podcast',
      limit: String(boundedLimit)
    }, 4 * 1024 * 1024);
    return invokePodcast<PodcastFeed[]>('podcast_parse_search', {
      payload,
      limit: boundedLimit
    });
  }

  async function showPodcasts() {
    activeTab = 'podcasts';
    error = '';
    if (podcastFeeds.length === 0 && !selectedPodcast) await loadTrendingPodcasts();
  }

  async function loadTrendingPodcasts() {
    if (podcastLoading) return;
    const viewVersion = ++podcastViewVersion;
    podcastLoading = true;
    showingLikedPodcasts = false;
    podcastGenre = '';
    error = '';
    selectedPodcast = null;
    podcastEpisodes = [];
    try {
      const results = await searchPodcastDirectory('podcast', 30);
      if (viewVersion === podcastViewVersion) podcastFeeds = results;
    } catch (nextError) {
      if (viewVersion === podcastViewVersion) {
        error = String(nextError);
        podcastFeeds = [];
      }
    } finally {
      if (viewVersion === podcastViewVersion) podcastLoading = false;
    }
  }

  async function searchPodcasts() {
    const query = podcastQuery.trim();
    if (!query) return loadTrendingPodcasts();
    if (podcastLoading) return;
    const viewVersion = ++podcastViewVersion;
    podcastLoading = true;
    showingLikedPodcasts = false;
    podcastGenre = '';
    error = '';
    selectedPodcast = null;
    podcastEpisodes = [];
    try {
      const results = await searchPodcastDirectory(query, 50);
      if (viewVersion === podcastViewVersion) podcastFeeds = results;
    } catch (nextError) {
      if (viewVersion === podcastViewVersion) {
        error = String(nextError);
        podcastFeeds = [];
      }
    } finally {
      if (viewVersion === podcastViewVersion) podcastLoading = false;
    }
  }

  function showLikedPodcastList() {
    podcastViewVersion += 1;
    podcastLoading = false;
    showingLikedPodcasts = !showingLikedPodcasts;
    selectedPodcast = null;
    podcastEpisodes = [];
    podcastGenre = '';
    podcastQuery = '';
    if (showingLikedPodcasts) {
      podcastFeeds = [...likedPodcasts];
    } else {
      void loadTrendingPodcasts();
    }
  }

  async function selectPodcastGenre(genre: string) {
    if (podcastLoading) return;
    if (podcastGenre === genre && !showingLikedPodcasts) {
      await loadTrendingPodcasts();
      return;
    }
    const viewVersion = ++podcastViewVersion;
    podcastLoading = true;
    showingLikedPodcasts = false;
    podcastGenre = genre;
    podcastQuery = '';
    selectedPodcast = null;
    podcastEpisodes = [];
    error = '';
    try {
      const results = await searchPodcastDirectory(genre, 50);
      if (viewVersion !== podcastViewVersion) return;
      const categoryMatches = results.filter((feed) => feedMatchesGenre(feed, genre));
      podcastFeeds = categoryMatches.length > 0 ? categoryMatches : results;
    } catch (nextError) {
      if (viewVersion === podcastViewVersion) {
        error = String(nextError);
        podcastFeeds = [];
      }
    } finally {
      if (viewVersion === podcastViewVersion) podcastLoading = false;
    }
  }

  async function openPodcast(feed: PodcastFeed) {
    if (podcastLoading) return;
    selectedPodcast = feed;
    podcastLoading = true;
    error = '';
    try {
      const directoryPayload = await fetchPodcastDirectory('lookup', {
        id: String(feed.id),
        media: 'podcast',
        entity: 'podcastEpisode',
        limit: '50'
      }, 8 * 1024 * 1024);
      podcastEpisodes = await invokePodcast<PodcastEpisode[]>('podcast_episodes', {
        feed,
        directoryPayload
      });
    } catch (nextError) {
      error = String(nextError);
      podcastEpisodes = [];
    } finally {
      podcastLoading = false;
    }
  }

  function rememberPodcast(episode: PodcastEpisode) {
    podcastHistory = [episode, ...podcastHistory.filter((item) => item.id !== episode.id)].slice(0, 10);
    window.localStorage.setItem('napstrfy-podcast-history', JSON.stringify(podcastHistory));
  }

  async function playPodcast(episode: PodcastEpisode) {
    if (caching) return;
    caching = true;
    error = '';
    try {
      audio?.pause();
      const source = await invoke<{ url: string; downloaded: boolean }>('podcast_playback_url', { episode });
      activeMedia = 'podcast';
      currentPodcast = episode;
      currentTime = 0;
      duration = episode.duration || 0;
      audio.src = source.url;
      audio.volume = volume;
      await audio.play();
      rememberPodcast(episode);
    } catch (nextError) {
      playing = false;
      error = `Could not play ${episode.title}: ${String(nextError)}`;
    } finally {
      caching = false;
    }
  }

  async function downloadPodcast(episode: PodcastEpisode) {
    const existing = podcastDownloadFor(episode.id);
    if (existing?.ready || existing?.status === 'Downloading') return;
    error = '';
    try {
      await invoke('podcast_download', { episode });
      await refreshPodcastDownloads();
      notice = `${episode.title} is downloading for offline listening`;
    } catch (nextError) {
      error = `Could not download ${episode.title}: ${String(nextError)}`;
    }
  }

  async function refreshPodcastDownloads() {
    try {
      podcastDownloads = await invoke<PodcastDownload[]>('podcast_downloads');
    } catch { /* the next foreground poll retries */ }
  }

  function hasActivePodcastDownload() {
    return podcastDownloads.some((download) => !download.ready && /downloading/i.test(download.status));
  }

  onMount(() => {
    const savedPlayMode = window.localStorage.getItem('napstrfy-play-mode');
    if (playModes.some((mode) => mode.value === savedPlayMode)) playMode = savedPlayMode as PlayMode;
    try {
      const saved = JSON.parse(window.localStorage.getItem(likedMusicKey) || '[]') as unknown;
      if (Array.isArray(saved)) likedMusic = saved.filter(isStoredTrack).slice(0, 1000);
    } catch { likedMusic = []; }
    try {
      const saved = JSON.parse(window.localStorage.getItem(likedPodcastsKey) || '[]') as unknown;
      if (Array.isArray(saved)) {
        likedPodcasts = saved.filter(isStoredPodcast).slice(0, 500).map((feed) => ({
          ...feed,
          genres: Array.isArray(feed.genres) ? feed.genres.filter((genre) => typeof genre === 'string').slice(0, 12) : []
        }));
      }
    } catch { likedPodcasts = []; }
    try {
      const saved = JSON.parse(window.localStorage.getItem('napstrfy-podcast-history') || window.localStorage.getItem('nostrfy-podcast-history') || '[]');
      if (Array.isArray(saved)) podcastHistory = saved.slice(0, 10);
    } catch { podcastHistory = []; }
    void loadCachedLibrary()
      .then(() => refreshStatus(true, false))
      .then(() => { if (status.connected) void loadLibrary(); });
    void refreshPodcastDownloads();
    const statusTimer = window.setInterval(() => {
      if (!document.hidden) void refreshStatus();
    }, 15000);
    const transferTimer = window.setInterval(() => {
      if (!document.hidden && pending.size > 0) void refreshTransfers();
    }, 3000);
    const podcastTimer = window.setInterval(() => {
      if (!document.hidden && hasActivePodcastDownload()) void refreshPodcastDownloads();
    }, 2500);
    const foreground = () => {
      if (document.hidden) return;
      void refreshStatus();
      void refreshTransfers();
      void refreshPodcastDownloads();
    };
    document.addEventListener('visibilitychange', foreground);
    window.addEventListener('napstrfy-media-action', handleSystemMediaAction);
    return () => {
      window.clearInterval(statusTimer);
      window.clearInterval(transferTimer);
      window.clearInterval(podcastTimer);
      document.removeEventListener('visibilitychange', foreground);
      window.removeEventListener('napstrfy-media-action', handleSystemMediaAction);
      androidMediaBridge()?.clear();
    };
  });
</script>

<svelte:head><title>Napstrfy</title></svelte:head>

{#if !status.paired && activeTab !== 'podcasts'}
  <main class="pair-screen">
    <div class="pair-glow"></div>
    <div class="pair-logo" aria-label="Napstrfy"><img src="/favicon.png" alt="" /><span>napstrfy</span></div>
    <p class="eyebrow">NAPSTR COMPANION</p>
    <h1>Your music.<br />Wherever you are.</h1>
    <p class="pair-copy">Pair securely with Napstr on your computer. Discovery and Tor downloads stay there; your music reaches this phone over encrypted Iroh.</p>
    {#if error}
      <div class="error-card">
        <span>{error}</span>
        {#if cameraPermissionDenied}<button onclick={showCameraSettings}>Open app settings</button>{/if}
      </div>
    {/if}
    <button class="scan-button" onclick={scanCode} disabled={scanning || pairing || statusLoading}><span>▦</span>{scanning ? 'Opening camera…' : pairing ? 'Pairing…' : 'Scan Napstr QR'}</button>
    <button class="browse-podcasts" onclick={showPodcasts}>Listen to podcasts without pairing</button>
    <details class="manual-pair">
      <summary>Enter a pairing code instead</summary>
      <textarea bind:value={pairingCode} placeholder="napstrfy://pair/…"></textarea>
      <button onclick={() => pair()} disabled={!pairingCode.trim() || pairing}>Connect</button>
    </details>
    <small class="pair-security">One-use pairing · no Nostr keys leave your computer</small>
  </main>
{:else}
  <main class="app-shell">
    <header class="mobile-header">
      <div class="brand"><img src="/napstr-logo-small.png" alt="" /><b>napstrfy</b></div>
      {#if status.paired}
        <button class="desktop-status" class:offline={!status.connected} onclick={reconnect}><i></i><span>{statusPending ? 'Connecting…' : status.connected ? status.desktopName || 'Napstr connected' : 'Reconnect'}</span></button>
      {:else}
        <button class="desktop-status offline" onclick={() => (activeTab = 'music')}><i></i><span>Pair Napstr for music</span></button>
      {/if}
    </header>

    {#if error}<button class="error-banner" onclick={() => (error = '')}>{error}<span>×</span></button>{/if}
    {#if notice}<button class="notice-banner" onclick={() => (notice = '')}>{notice}<span>×</span></button>{/if}

    {#if activeTab === 'music'}
      <section class="search-area">
        <form onsubmit={(event) => { event.preventDefault(); void searchTracks(); }}>
          <span>⌕</span><input bind:value={query} placeholder="Search your music and Nostr" aria-label="Search tracks" />
          {#if query}<button type="button" class="clear-search" onclick={() => searchTracks('')}>×</button>{/if}
        </form>
        <div class="chips"><button class:active={showingLikedMusic} onclick={showLikedTracks}>♥ Liked</button>{#each musicChips as chip}<button class:active={!showingLikedMusic && query.toLocaleLowerCase() === chip.toLocaleLowerCase()} onclick={() => selectChip(chip)}>{chip}</button>{/each}</div>
      </section>

      <section class="library-heading">
        <div><p>{showingLikedMusic ? 'FAVOURITES' : query ? 'SEARCH RESULTS' : 'YOUR NAPSTR'}</p><h1>{showingLikedMusic ? 'Liked music' : query ? query : 'Your music'}</h1></div>
        <span>{total} {total === 1 ? 'track' : 'tracks'}</span>
      </section>

      <section class="track-list" aria-busy={loading}>
        {#if loading}<div class="loading-list"><i></i><span>Asking Napstr…</span></div>{/if}
        {#if !loading && tracks.length === 0}<div class="empty-library"><img src="/napstr-logo-small.png" alt="" /><h2>{showingLikedMusic ? 'No liked tracks yet' : 'No tracks found'}</h2><p>{showingLikedMusic ? 'Tap the heart beside a song to keep it here.' : query ? 'Try different words or clear the search.' : 'Add music to your Napstr folder on the computer.'}</p></div>{/if}
        {#each tracks as track, index (track.fileId)}
          <div class:selected={selected?.fileId === track.fileId} class:remote={!track.local} class="track-row">
            <button class="track-open" onclick={() => activateTrack(track)}>
              <TrackArtwork {track} lookup={index < 24} />
              <span class="track-copy"><strong>{title(track)}</strong><small>{artist(track)}{track.album ? ` · ${track.album}` : ''}</small></span>
              <span class="track-meta">{track.local ? readableSize(track.size) : `${track.sources.length} ${track.sources.length === 1 ? 'seeder' : 'seeders'}`}</span>
              <span class="track-action">{pending.has(track.fileId) ? '···' : track.local ? '⋮' : '⇩'}</span>
            </button>
            <button class:liked={isTrackLiked(track)} class="like-button" onclick={() => toggleTrackLike(track)} aria-label={`${isTrackLiked(track) ? 'Unlike' : 'Like'} ${title(track)}`}>{isTrackLiked(track) ? '♥' : '♡'}</button>
          </div>
        {/each}
        {#if !showingLikedMusic && tracks.length < total}<button class="load-more" onclick={() => loadLibrary(true)} disabled={loadingMore}>{loadingMore ? 'Loading…' : `Load more · ${tracks.length} of ${total}`}</button>{/if}
      </section>
    {:else if activeTab === 'podcasts'}
      <section class="search-area podcast-search">
        <form onsubmit={(event) => { event.preventDefault(); void searchPodcasts(); }}>
          <span>⌕</span><input bind:value={podcastQuery} placeholder="Search podcasts" aria-label="Search podcasts" />
          {#if podcastQuery}<button type="button" class="clear-search" onclick={() => { podcastQuery = ''; void loadTrendingPodcasts(); }}>×</button>{/if}
        </form>
        <div class="chips podcast-genres"><button class:active={showingLikedPodcasts} onclick={showLikedPodcastList}>♥ Liked</button>{#each podcastGenres as genre}<button class:active={!showingLikedPodcasts && podcastGenre === genre} onclick={() => selectPodcastGenre(genre)}>{genre}</button>{/each}</div>
      </section>

      {#if selectedPodcast}
        <section class="podcast-show-heading">
          <button class="podcast-back" onclick={() => { selectedPodcast = null; podcastEpisodes = []; }}>‹</button>
          {#if selectedPodcast.image}<img src={selectedPodcast.image} alt="" />{:else}<div class="podcast-art-fallback">◉</div>{/if}
          <div><p>PODCAST</p><h1>{selectedPodcast.title}</h1><small>{selectedPodcast.author || 'Independent podcast'}</small></div>
          <button class:liked={isPodcastLiked(selectedPodcast)} class="like-button podcast-heading-like" onclick={() => togglePodcastLike(selectedPodcast!)} aria-label={`${isPodcastLiked(selectedPodcast) ? 'Unlike' : 'Like'} ${selectedPodcast.title}`}>{isPodcastLiked(selectedPodcast) ? '♥' : '♡'}</button>
        </section>
        <section class="episode-list" aria-busy={podcastLoading}>
          {#if podcastLoading}<div class="loading-list"><i></i><span>Loading episodes…</span></div>{/if}
          {#each podcastEpisodes as episode (episode.id)}
            {@const download = podcastDownloadFor(episode.id)}
            <article class="episode-row">
              <button class="episode-play" onclick={() => playPodcast(episode)}>▶</button>
              <button class="episode-copy" onclick={() => playPodcast(episode)}><strong>{episode.title}</strong><small>{podcastDate(episode.datePublished)}{episode.duration ? ` · ${clock(episode.duration)}` : ''}</small></button>
              <button class:ready={download?.ready} class="episode-download" onclick={() => downloadPodcast(episode)} disabled={download?.status === 'Downloading'} aria-label={`Download ${episode.title}`} title={download?.status || 'Download for offline listening'}>{download?.ready ? '✓' : download?.status === 'Downloading' ? `${Math.round(download.progress)}%` : '⇩'}</button>
            </article>
          {/each}
          {#if !podcastLoading && podcastEpisodes.length === 0}<div class="empty-library"><h2>No playable episodes</h2><p>This feed may not currently expose supported HTTPS audio.</p></div>{/if}
        </section>
      {:else}
        {#if podcastHistory.length > 0 && !podcastQuery && !podcastGenre && !showingLikedPodcasts}
          <section class="podcast-history"><div class="section-label"><b>Recently played</b><span>Last 10</span></div><div class="history-scroller">{#each podcastHistory as episode (episode.id)}<button onclick={() => playPodcast(episode)}>{#if episode.image}<img src={episode.image} alt="" />{:else}<span>◉</span>{/if}<strong>{episode.title}</strong><small>{episode.feedTitle}</small></button>{/each}</div></section>
        {/if}
        <section class="library-heading">
          <div><p>{showingLikedPodcasts ? 'FAVOURITES' : 'POWERED BY PODCAST INDEX'}</p><h1>{showingLikedPodcasts ? 'Liked podcasts' : podcastGenre ? podcastGenre : podcastQuery ? `Results for “${podcastQuery}”` : 'Discover podcasts'}</h1></div>
          <span>{podcastFeeds.length} shows</span>
        </section>
        <section class="podcast-grid" aria-busy={podcastLoading}>
          {#if podcastLoading}<div class="loading-list"><i></i><span>Searching podcasts…</span></div>{/if}
          {#each podcastFeeds as feed (feed.id)}
            <article class="podcast-card">
              <button class="podcast-open" onclick={() => openPodcast(feed)}>
                {#if feed.image}<img src={feed.image} alt="" />{:else}<div class="podcast-art-fallback">◉</div>{/if}
                <span><strong>{feed.title}</strong><small>{feed.author || 'Independent podcast'}</small></span>
              </button>
              <button class:liked={isPodcastLiked(feed)} class="like-button podcast-like" onclick={() => togglePodcastLike(feed)} aria-label={`${isPodcastLiked(feed) ? 'Unlike' : 'Like'} ${feed.title}`}>{isPodcastLiked(feed) ? '♥' : '♡'}</button>
            </article>
          {/each}
          {#if !podcastLoading && podcastFeeds.length === 0}<div class="empty-library"><h2>{showingLikedPodcasts ? 'No liked podcasts yet' : 'Search podcasts'}</h2><p>{showingLikedPodcasts ? 'Tap the heart beside a podcast to keep it here.' : 'Napstrfy searches podcasts directly over this phone\'s internet connection.'}</p></div>{/if}
        </section>
      {/if}
    {:else}
      <section class="search-area audiobook-search">
        <form onsubmit={(event) => { event.preventDefault(); void loadAudiobooks(); }}>
          <span>⌕</span><input bind:value={audiobookQuery} placeholder="Search audiobooks" aria-label="Search audiobooks" />
          {#if audiobookQuery}<button type="button" class="clear-search" onclick={() => { audiobookQuery = ''; void loadAudiobooks(); }}>×</button>{/if}
        </form>
      </section>

      {#if selectedAudiobook}
        <section class="audiobook-show-heading">
          <button class="podcast-back" onclick={() => (selectedAudiobook = null)}>‹</button>
          <div class="audiobook-cover">▥</div>
          <div><p>AUDIOBOOK</p><h1>{selectedAudiobook.title}</h1><small>{selectedAudiobook.author || 'Unknown author'}{selectedAudiobook.narrator ? ` · Read by ${selectedAudiobook.narrator}` : ''}</small></div>
        </section>
        <section class="audiobook-chapter-list" aria-busy={audiobookLoading}>
          {#each selectedAudiobook.chapters as chapter, index (chapter.fileId)}
            <button class="audiobook-chapter" onclick={() => activateAudiobookChapter(selectedAudiobook!, chapter)}>
              <span>{chapter.local ? '▶' : '⇩'}</span>
              <span><strong>{chapter.title || chapter.filename}</strong><small>Chapter {index + 1} · {readableSize(chapter.size)}</small></span>
            </button>
          {/each}
        </section>
      {:else}
        <section class="library-heading">
          <div><p>YOUR NAPSTR</p><h1>Audiobooks</h1></div>
          <span>{audiobookTotal} {audiobookTotal === 1 ? 'book' : 'books'}</span>
        </section>
        <section class="audiobook-list" aria-busy={audiobookLoading}>
          {#if audiobookLoading}<div class="loading-list"><i></i><span>Asking Napstr…</span></div>{/if}
          {#each audiobooks as book (book.audiobookId)}
            <button class="audiobook-card" onclick={() => openAudiobook(book)}>
              <span class="audiobook-cover">▥</span>
              <span><strong>{book.title}</strong><small>{book.author || 'Unknown author'}</small><i>{book.chapterCount} {book.chapterCount === 1 ? 'file' : 'chapters'} · {readableSize(book.totalSize)}</i></span>
              <b>›</b>
            </button>
          {/each}
          {#if !audiobookLoading && audiobooks.length === 0}<div class="empty-library"><h2>No audiobooks found</h2><p>Group a chapter folder in Napstr, or add the tag “audiobook” to a complete one-file book.</p></div>{/if}
        </section>
      {/if}
    {/if}

    <nav class="bottom-nav" aria-label="Napstrfy navigation">
      <button class:active={activeTab === 'music'} onclick={() => (activeTab = 'music')}><span>♫</span>Music</button>
      <button class:active={activeTab === 'podcasts'} onclick={showPodcasts}><span>◉</span>Podcasts</button>
      <button class:active={activeTab === 'audiobooks'} onclick={showAudiobooks}><span>▥</span>Audiobooks</button>
      <button onclick={() => status.paired ? forgetDesktop() : (activeTab = 'music')}><span>⚙</span>Pairing</button>
    </nav>

    <section class:empty={activeMedia === 'music' ? !current : !currentPodcast} class="now-playing">
      {#if activeMedia === 'podcast' && currentPodcast}
        {#if currentPodcast.image}<img class="podcast-player-art" src={currentPodcast.image} alt="" />{:else}<div class="empty-art">◉</div>{/if}
      {:else if current}<TrackArtwork track={current} large lookup />{:else}<div class="empty-art">♪</div>{/if}
      <div class="now-copy"><strong>{activeMedia === 'podcast' && currentPodcast ? currentPodcast.title : current ? title(current) : 'Choose something to play'}</strong><small>{activeMedia === 'podcast' && currentPodcast ? currentPodcast.feedTitle : current ? artist(current) : 'Music and podcasts, wherever you are'}</small></div>
      <div class="timeline"><input type="range" min="0" max={duration || 0} step="0.1" value={currentTime} oninput={(event) => seek(Number(event.currentTarget.value))} disabled={!current && !currentPodcast} /><span>{clock(currentTime)} / {clock(duration)}</span></div>
      <div class="player-buttons">
        <button onclick={() => moveTrack(-1)} disabled={activeMedia !== 'music' || playerQueue.length < 2 || (playMode === 'random' && randomHistoryIndex <= 0)} aria-label="Previous track">|◀</button>
        <button class="play-main" onclick={togglePlayer} disabled={(!current && !currentPodcast) || caching}>{caching ? '···' : playing ? 'Ⅱ' : '▶'}</button>
        <button onclick={() => moveTrack(1)} disabled={activeMedia !== 'music' || playerQueue.length < 2} aria-label="Next track">▶|</button>
        <button class="mode-button" class:active={activeMedia === 'music'} onclick={cyclePlayMode} disabled={activeMedia !== 'music'} aria-label={playModeDetails().label} title={playModeDetails().label}>{playModeDetails().icon}</button>
      </div>
      <label class="volume">⌁ <input type="range" min="0" max="1" step="0.02" value={volume} oninput={(event) => setVolume(Number(event.currentTarget.value))} /></label>
    </section>
  </main>
{/if}

<audio
  bind:this={audio}
  onplay={() => { playing = true; syncSystemMedia(true); }}
  onpause={() => { playing = false; syncSystemMedia(true); }}
  ontimeupdate={() => { currentTime = audio.currentTime; syncSystemMedia(); }}
  ondurationchange={() => { duration = Number.isFinite(audio.duration) ? audio.duration : 0; syncSystemMedia(true); }}
  onended={handleTrackEnded}
  onerror={() => {
    if (activeMedia === 'podcast' && currentPodcast) error = `This phone could not play ${currentPodcast.title}.`;
    else if (current) error = `This phone could not decode ${current.format} audio.`;
  }}
></audio>
