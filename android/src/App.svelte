<script lang="ts">
  import { t, locale, message as msg, initializeLocale, type Message } from '@napstr/i18n/svelte';
  import LanguageSelect from '@napstr/i18n/LanguageSelect.svelte';
  import { readStoredPreference } from '@napstr/i18n';
  import { locale as osLocale } from '@tauri-apps/plugin-os';
  import '@napstr/i18n/styles.css';
  import { onMount, tick, untrack } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import {
    Format,
    checkPermissions,
    openAppSettings,
    requestPermissions,
    scan
  } from '@tauri-apps/plugin-barcode-scanner';
  import TrackArtwork from './lib/TrackArtwork.svelte';
  import SeekIcon from './lib/SeekIcon.svelte';
  import { durationMonitor, rateLimitedTask, validDuration, safePosition } from './lib/playback';
  import appIcon from '../src-tauri/icons/icon.png';
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
  let platform = $state('');
  const mobile = $derived(platform === 'android' || platform === 'ios');
  let manualPairOpen = $state(true);
  $effect(() => { manualPairOpen = !mobile; });
  let pairingDialog: HTMLDialogElement;
  let status = $state<CompanionStatus>({ streamOnly: false, paired: false, connected: false, desktopName: '', endpointId: '', libraryRevision: 0, error: '' });
  let statusLoading = $state(true);
  let statusPending = $state(false);
  let pairingCode = $state('');
  let pairing = $state(false);
  let scanning = $state(false);
  let cameraPermissionDenied = $state(false);
  let error = $state<string | Message>('');
  let notice = $state<string | Message>('');
  $effect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => { notice = ''; }, 10_000);
    return () => window.clearTimeout(timer);
  });
  $effect(() => {
    // Pairing errors can contain a camera-permission action; keep those visible.
    if (!error || (!status.paired && activeTab !== 'podcasts')) return;
    const timer = window.setTimeout(() => { error = ''; }, 10_000);
    return () => window.clearTimeout(timer);
  });
  let query = $state('');
  let tracks = $state<RemoteTrack[]>([]);
  let likedMusic = $state<RemoteTrack[]>([]);
  let showingLikedMusic = $state(false);
  let total = $state(0);
  let loading = $state(false);
  let loadingMore = $state(false);
  let searchingNetwork = $state(false);
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
  let durationHint = $state(0);
  const displayedDuration = $derived(duration || durationHint);
  const canSeek = $derived(duration > 0 && !caching);
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
  let currentPodcastFeed = $state<PodcastFeed | null>(null);
  let activeMedia = $state<'music' | 'podcast'>('music');
  const playerLiked = $derived(activeMedia === 'music'
    ? Boolean(current && isTrackLiked(current))
    : Boolean(currentPodcastFeed && isPodcastLiked(currentPodcastFeed)));
  const playerLikeTitle = $derived(activeMedia === 'music' ? current && title(current) : currentPodcastFeed?.title);
  let audio: HTMLAudioElement;
  let playbackSource = '';
  let lastSystemMetadata = '';
  let lastSystemPosition = '';
  let lastSystemState = '';
  let lastAndroidState = '';
  const mediaUpdates = rateLimitedTask(publishSystemMedia);
  const timing = durationMonitor((value) => {
    // This callback only changes UI state. System publishing runs independently.
    duration = value;
  });

  function resetPlaybackTiming() {
    timing.stop();
    playbackSource = '';
    duration = 0;
    durationHint = 0;
    currentTime = 0;
  }

  function setPlaybackSource(url: string, hint = 0) {
    resetPlaybackTiming();
    durationHint = validDuration(hint);
    audio.src = url;
    playbackSource = audio.src;
    const source = playbackSource;
    timing.start(() => audio.currentSrc === source ? audio.duration : 0);
  }

  function updatePlaybackPosition() {
    if (!playbackSource || audio.currentSrc !== playbackSource) return;
    currentTime = safePosition(audio.currentTime);
    syncSystemMedia();
  }
  let currentArtwork = $state('');
  const playerArtwork = $derived(activeMedia === 'podcast' ? currentPodcast?.image || '' : current ? currentArtwork : '');

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
    return track.artist || $t('Unknown artist');
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
      error = msg("Napstrfy could not save that favourite on this device.");
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

  function usePodcastArtwork(event: Event, fallback: string) {
    const image = event.currentTarget as HTMLImageElement;
    if (fallback && image.getAttribute('src') !== fallback) {
      image.src = fallback;
    } else {
      image.remove();
    }
  }

  function showLikedTracks() {
    musicViewVersion += 1;
    searchingNetwork = false;
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
    syncSystemMedia();
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
      const offline = await invoke<LibraryPage & { paired: boolean; desktopName: string; streamOnly: boolean }>('cached_library');
      if (offline.paired) {
        status = { ...status, paired: true, desktopName: offline.desktopName, streamOnly: offline.streamOnly };
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
      const device = ({ android: 'Android', ios: 'iPhone or iPad', windows: 'Windows', macos: 'Mac', linux: 'Linux' } as Record<string, string>)[platform] || 'this device';
      const desktop = await invoke<string>('pair_desktop', { code: code.trim(), deviceName: `Napstrfy on ${device}` });
      pairingCode = '';
      notice = msg("Connected to {p0}", { p0: desktop });
      await refreshStatus();
      await loadLibrary();
    } catch (nextError) {
      error = String(nextError);
    } finally {
      pairing = false;
    }
  }

  async function scanCode() {
    if (!mobile || scanning || pairing) return;
    error = '';
    cameraPermissionDenied = false;
    scanning = true;
    try {
      let permission = await checkPermissions();
      if (permission !== 'granted') permission = await requestPermissions();
      if (permission !== 'granted') {
        cameraPermissionDenied = true;
        error = msg("Camera access is required to scan the Napstr pairing code.");
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
          ? msg("Camera access is required to scan the Napstr pairing code.")
          : msg("Could not open the QR scanner: {p0}", { p0: message });
      }
    } finally {
      scanning = false;
    }
  }

  async function showCameraSettings() {
    try {
      await openAppSettings();
    } catch (nextError) {
      error = msg("Could not open app settings: {p0}", { p0: String(nextError) });
    }
  }

  async function forgetDesktop() {
    try {
      await invoke('forget_desktop');
      musicViewVersion += 1;
      loading = loadingMore = searchingNetwork = false;
      audio?.pause();
      resetPlaybackTiming();
      mediaUpdates.cancel();
      lastSystemMetadata = lastSystemPosition = lastSystemState = lastAndroidState = '';
      status = { streamOnly: false, paired: false, connected: false, desktopName: '', endpointId: '', libraryRevision: 0, error: '' };
      tracks = [];
      current = null;
      currentPodcast = null;
      playerQueue = [];
      activeTab = 'music';
      androidMediaBridge()?.clear();
      if ('mediaSession' in navigator) {
        navigator.mediaSession.metadata = null;
        navigator.mediaSession.playbackState = 'none';
      }
      pairingDialog.close();
    } catch (nextError) {
      pairingDialog.close();
      error = String(nextError);
    }
  }

  function showPairing() {
    if (!status.paired) { activeTab = 'music'; return; }
    pairingDialog.showModal();
  }

  async function loadLibrary(append = false) {
    if (!status.paired || (append && (loading || loadingMore))) return;
    const viewVersion = ++musicViewVersion;
    searchingNetwork = false;
    if (!append) loadingMore = false;
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
    silentLibraryRefresh = true;
    const viewVersion = musicViewVersion;
    try {
      if (showingLikedMusic || query.trim()) {
        await refreshTrackAvailability(tracks);
        if (viewVersion === musicViewVersion) loadedLibraryRevision = revision;
        return;
      }
      const page = await invoke<LibraryPage>('remote_library', { query: '', offset: 0, limit: 100 });
      if (viewVersion !== musicViewVersion) return;
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
    const viewVersion = ++musicViewVersion;
    const searchQuery = query.trim();
    loading = true;
    loadingMore = false;
    searchingNetwork = !status.streamOnly;
    tracks = [];
    total = 0;
    selected = null;
    error = '';
    const mergeResults = (results: RemoteTrack[]) => {
      if (viewVersion !== musicViewVersion) return;
      const merged = new Map(tracks.map((track) => [track.fileId, track]));
      for (const track of results) {
        // A network response must not downgrade a track already on the host.
        if (!merged.get(track.fileId)?.local || track.local) merged.set(track.fileId, track);
      }
      tracks = [...merged.values()].sort((left, right) => Number(right.local) - Number(left.local));
      total = tracks.length;
      selected = tracks.find((track) => track.fileId === selected?.fileId) ?? tracks[0] ?? null;
    };
    const failed = (nextError: unknown) => {
      if (viewVersion === musicViewVersion) error = String(nextError);
    };
    // Separate Iroh requests let host-library results arrive without waiting
    // for Nostr. Both use existing endpoints, including on older hosts.
    const localSearch = invoke<LibraryPage>('remote_library', { query: searchQuery, offset: 0, limit: 200 })
      .then((page) => mergeResults(page.tracks))
      .catch(failed)
      .finally(() => { if (viewVersion === musicViewVersion) loading = false; });
    const networkSearch = searchingNetwork
      ? invoke<RemoteTrack[]>('remote_search', { query: searchQuery })
        .then(async (results) => {
          mergeResults(results);
          if (viewVersion === musicViewVersion) {
            // A host download can finish after the search began. Check hashes,
            // since the publisher's title/filename may differ from local tags.
            await refreshTrackAvailability(results).catch(() => {});
          }
        })
        .catch(failed)
        .finally(() => { if (viewVersion === musicViewVersion) searchingNetwork = false; })
      : Promise.resolve();
    await Promise.all([localSearch, networkSearch]);
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
    resetPlaybackTiming();
    error = '';
    current = track;
    activeMedia = 'music';
    try {
      audio?.pause();
      const cached = await invoke<CachedAudio>('cache_remote_audio', { track, libraryVisible });
      current = cached.track;
      await tick();
      setPlaybackSource(cached.url);
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
      error = msg("Could not play {p0}: {p1}", { p0: title(track), p1: String(nextError) });
    } finally {
      caching = false;
      syncSystemMedia();
    }
  }

  function audiobookDestinationFolder(book: RemoteAudiobook) {
    const title = book.title
      .replace(/[\x00-\x1f/\\:*?"<>|]/g, '_')
      .replace(/^[.\s]+|[.\s]+$/g, '')
      .slice(0, 86) || 'Audiobook';
    return `${title} [${book.audiobookId.slice(0, 8)}]`;
  }

  async function refreshTrackAvailability(candidates: RemoteTrack[]) {
    const viewVersion = musicViewVersion;
    const endpointId = status.endpointId;
    const ids = [...new Set(candidates.filter((track) => !track.local).map((track) => track.fileId))];
    const available = new Set<string>();
    for (let offset = 0; offset < ids.length; offset += 200) {
      const batch = ids.slice(offset, offset + 200);
      const confirmed = await invoke<string[]>('remote_available', { fileIds: batch });
      for (const fileId of confirmed) if (batch.includes(fileId)) available.add(fileId);
    }
    if (viewVersion !== musicViewVersion || endpointId !== status.endpointId) return new Set<string>();
    const promote = (track: RemoteTrack) => available.has(track.fileId) ? { ...track, local: true } : track;
    tracks = tracks.map(promote);
    if (selected) selected = promote(selected);
    if (selectedAudiobook) selectedAudiobook = { ...selectedAudiobook, chapters: selectedAudiobook.chapters.map(promote) };
    if (likedMusic.some((track) => !track.local && available.has(track.fileId))) {
      likedMusic = likedMusic.map(promote);
      saveLikes(likedMusicKey, likedMusic);
    }
    return available;
  }

  function clearPendingDownload(fileId: string) {
    const next = new Map(pending);
    next.delete(fileId);
    pending = next;
    const nextAudiobooks = new Map(pendingAudiobooks);
    nextAudiobooks.delete(fileId);
    pendingAudiobooks = nextAudiobooks;
  }

  async function playIfAvailable(track: RemoteTrack, audiobookId: string | null) {
    const available = await refreshTrackAvailability([track]).catch(() => new Set<string>());
    if (!available.has(track.fileId)) return false;
    clearPendingDownload(track.fileId);
    if (activeMedia === 'music' && selected?.fileId === track.fileId) {
      const local = { ...track, local: true };
      if (audiobookId && selectedAudiobook?.audiobookId === audiobookId) {
        await activateAudiobookChapter(selectedAudiobook, local);
      } else if (!audiobookId) {
        await activateTrack(local);
      }
    }
    return true;
  }

  async function requestDownload(
    track: RemoteTrack,
    destinationFolder: string | null = null,
    audiobookId: string | null = null
  ) {
    if (status.streamOnly) {
      error = msg("This pairing is read only. It cannot ask Napstr to download songs.");
      return;
    }
    if (pending.has(track.fileId)) return;
    pending = new Map(pending).set(track.fileId, track.filename);
    if (audiobookId) pendingAudiobooks = new Map(pendingAudiobooks).set(track.fileId, audiobookId);
    error = '';
    try {
      if (await playIfAvailable(track, audiobookId)) return;
      await invoke<string>('remote_download', {
        fileId: track.fileId,
        sourcePubkeys: track.sources.map((source) => source.pubkey),
        destinationFolder
      });
      notice = msg("Napstr is downloading {p0} over Tor", { p0: title(track) });
      await refreshTransfers();
    } catch (nextError) {
      // Close the race where the host finished between our check and request.
      if (await playIfAvailable(track, audiobookId)) return;
      clearPendingDownload(track.fileId);
      error = String(nextError);
    }
  }

  async function refreshTransfers() {
    if (!status.connected || status.streamOnly || pending.size === 0) return;
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
          error = msg("{p0}: {p1}", { p0: transfer.filename, p1: transfer.status });
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
          const available = original ? await refreshTrackAvailability([original]).catch(() => null) : null;
          if (original && (original.local || available?.has(pendingFileId))) {
            local = { ...original, local: true };
          } else if (!available) {
            // Retain compatibility with hosts predating hash availability.
            const page = await invoke<LibraryPage>('remote_library', { query: original?.filename || transfer?.filename || pendingFilename, offset: 0, limit: 20 });
            local = page.tracks.find((item) => item.fileId === pendingFileId);
          }
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
        notice = msg("{p0} is ready to play", { p0: title(local) });
      }
    } catch { /* the next foreground poll retries */ }
  }

  function togglePlayer() {
    if ((!current && !currentPodcast) || caching) return;
    if (audio.paused) audio.play().catch((nextError) => (error = String(nextError)));
    else audio.pause();
  }

  function syncSystemMedia() {
    mediaUpdates.request();
  }

  function publishSystemMedia() {
    const bridge = androidMediaBridge();
    const media = activeMedia === 'podcast' ? currentPodcast : current;
    if (!media || caching) return;
    const metadata = {
      title: activeMedia === 'podcast' ? currentPodcast?.title : current ? title(current) : '',
      artist: activeMedia === 'podcast' ? currentPodcast?.feedTitle : current ? artist(current) : '',
      labels: { previous: $t('Previous track'), rewind: $t('Back 15 seconds'), play: $t('Play'), pause: $t('Pause'), forward: $t('Forward 15 seconds'), next: $t('Next track'), channel: $t('Media playback') },
      canSeek,
      playing,
      position: safePosition(currentTime, duration || undefined),
      duration: validDuration(duration),
      canPrevious: activeMedia === 'music' && playerQueue.length > 1 && (playMode !== 'random' || randomHistoryIndex > 0),
      canNext: activeMedia === 'music' && playerQueue.length > 1
    };
    if (bridge) {
      const payload = JSON.stringify(metadata);
      if (payload !== lastAndroidState) {
        lastAndroidState = payload;
        bridge.update(payload);
      }
    } else if ('mediaSession' in navigator) {
      const session = navigator.mediaSession;
      try {
        // An estimate is for display only. Do not publish unknown or invalid
        // timing through WebKit's native desktop media conversions.
        if (!metadata.duration) {
          if (lastSystemPosition) {
            lastSystemPosition = '';
            lastSystemMetadata = '';
            lastSystemState = '';
            session.setPositionState();
            session.metadata = null;
            session.playbackState = 'none';
          }
          return;
        }
        const position = { duration: metadata.duration, playbackRate: 1, position: metadata.position };
        const positionKey = JSON.stringify(position);
        if (positionKey !== lastSystemPosition) {
          session.setPositionState(position);
          lastSystemPosition = positionKey;
        }
        const key = JSON.stringify([activeMedia, playbackSource, metadata.title, metadata.artist]);
        if (key !== lastSystemMetadata && 'MediaMetadata' in window) {
          session.metadata = new MediaMetadata({ title: metadata.title, artist: metadata.artist });
          lastSystemMetadata = key;
        }
        const state = playing ? 'playing' : 'paused';
        if (state !== lastSystemState) {
          session.playbackState = state;
          lastSystemState = state;
        }
      } catch { /* Some webviews expose only part of Media Session. */ }
    }
  }

  function setupMediaSession() {
    if (androidMediaBridge() || !('mediaSession' in navigator)) return () => {};
    const handlers: Array<[MediaSessionAction, MediaSessionActionHandler]> = [
      ['play', () => { if (audio?.paused) togglePlayer(); }],
      ['pause', () => audio?.pause()],
      ['previoustrack', () => { void moveTrack(-1); }],
      ['nexttrack', () => { void moveTrack(1); }],
      ['seekto', (event) => { if (event.seekTime !== undefined) seek(event.seekTime); }],
      ['seekbackward', (event) => skipSeconds(-(event.seekOffset ?? 15))],
      ['seekforward', (event) => skipSeconds(event.seekOffset ?? 15)]
    ];
    const registered: MediaSessionAction[] = [];
    for (const [action, handler] of handlers) {
      try {
        navigator.mediaSession.setActionHandler(action, handler);
        registered.push(action);
      } catch { /* Unsupported actions must not prevent playback. */ }
    }
    return () => {
      for (const action of registered) navigator.mediaSession.setActionHandler(action, null);
    };
  }

  function handleKeyboard(event: KeyboardEvent) {
    if (event.defaultPrevented || event.isComposing || pairingDialog?.open) return;
    if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === 'f') {
      const search = document.querySelector<HTMLInputElement>('.search-area input');
      if (search) { event.preventDefault(); search.focus(); search.select(); }
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.repeat) return;
    if (event.target instanceof HTMLElement && event.target.closest('input, textarea, select, button, a, summary, [contenteditable="true"]')) return;
    if (!current && !currentPodcast) return;
    if (event.code === 'Space') { event.preventDefault(); togglePlayer(); }
    else if (event.key === 'ArrowLeft') { event.preventDefault(); skipSeconds(-15); }
    else if (event.key === 'ArrowRight') { event.preventDefault(); skipSeconds(15); }
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
    } else if (action === 'rewind') {
      skipSeconds(-15);
    } else if (action === 'forward') {
      skipSeconds(15);
    } else if (action.startsWith('seek:')) {
      const milliseconds = Number(action.slice(5));
      if (Number.isFinite(milliseconds)) seek(milliseconds / 1000);
    }
  }

  function seek(value: number) {
    if (!audio || !canSeek || !playbackSource || audio.currentSrc !== playbackSource || !Number.isFinite(value)) return;
    const target = safePosition(value, duration);
    audio.currentTime = target;
    currentTime = target;
    syncSystemMedia();
  }

  function skipSeconds(offset: number) {
    if (audio) seek(audio.currentTime + offset);
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
    syncSystemMedia();
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
    return new Intl.DateTimeFormat($locale, { day: 'numeric', month: 'short', year: 'numeric' })
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
            () => reject(new Error($t('The podcast service did not respond. Check your connection and retry.'))),
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
      if (!response.ok) throw new Error($t("Podcast directory returned {p0}.", { p0: response.status }));
      const advertisedLength = Number(response.headers.get('content-length') || 0);
      if (advertisedLength > maximumBytes) throw new Error($t('Podcast directory response is too large.'));
      const payload = await response.text();
      if (new TextEncoder().encode(payload).byteLength > maximumBytes) {
        throw new Error($t('Podcast directory response is too large.'));
      }
      return payload;
    } catch (nextError) {
      if (controller.signal.aborted) {
        throw new Error($t('The podcast directory did not respond. Check your connection and retry.'));
      }
      throw nextError;
    } finally {
      window.clearTimeout(timeout);
    }
  }

  async function searchPodcastDirectory(searchTerm: string, limit: number): Promise<PodcastFeed[]> {
    const query = searchTerm.trim();
    if (!query || query.length > 120) throw new Error($t('Search for between 1 and 120 characters.'));
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
    resetPlaybackTiming();
    error = '';
    try {
      audio?.pause();
      const source = await invoke<{ url: string; downloaded: boolean }>('podcast_playback_url', { episode });
      activeMedia = 'podcast';
      currentPodcast = episode;
      currentPodcastFeed = [selectedPodcast, currentPodcastFeed, ...likedPodcasts, ...podcastFeeds]
        .find((feed) => feed?.id === episode.feedId) ?? null;
      setPlaybackSource(source.url, episode.duration);
      audio.volume = volume;
      await audio.play();
      rememberPodcast(episode);
    } catch (nextError) {
      playing = false;
      error = msg("Could not play {p0}: {p1}", { p0: episode.title, p1: String(nextError) });
    } finally {
      caching = false;
      syncSystemMedia();
    }
  }

  async function downloadPodcast(episode: PodcastEpisode) {
    const existing = podcastDownloadFor(episode.id);
    if (existing?.ready || existing?.status === 'Downloading') return;
    error = '';
    try {
      await invoke('podcast_download', { episode });
      // The worker has started. Keep polling even if this first refresh fails.
      podcastDownloads = [
        ...podcastDownloads.filter((download) => download.episode.id !== episode.id),
        { episode, progress: 0, status: 'Downloading', ready: false }
      ];
      await refreshPodcastDownloads();
      if (podcastDownloadFor(episode.id)?.status === 'Downloading') {
        notice = msg("{p0} is downloading for offline listening", { p0: episode.title });
      }
    } catch (nextError) {
      error = msg("Could not download {p0}: {p1}", { p0: episode.title, p1: String(nextError) });
    }
  }

  async function refreshPodcastDownloads() {
    try {
      const downloads = await invoke<PodcastDownload[]>('podcast_downloads');
      const failed = downloads.find((download) => download.status.startsWith('Failed:')
        && podcastDownloadFor(download.episode.id)?.status !== download.status);
      podcastDownloads = downloads;
      if (failed) {
        notice = '';
        error = msg("Could not download {p0}: {p1}", {
          p0: failed.episode.title, p1: failed.status.slice('Failed:'.length).trim()
        });
      }
    } catch { /* the next foreground poll retries */ }
  }

  function hasActivePodcastDownload() {
    return podcastDownloads.some((download) => !download.ready && /downloading/i.test(download.status));
  }

  onMount(() => initializeLocale(() => osLocale()));
  $effect(() => { $locale; untrack(() => syncSystemMedia()); });
  $effect(() => { duration; canSeek; untrack(() => syncSystemMedia()); });

  onMount(() => {
    void invoke<string>('client_platform').then((value) => { platform = value; }).catch(() => {});
    const clearMediaSession = setupMediaSession();
    const savedPlayMode = readStoredPreference('napstrfy-play-mode');
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
    window.addEventListener('keydown', handleKeyboard);
    return () => {
      window.clearInterval(statusTimer);
      window.clearInterval(transferTimer);
      window.clearInterval(podcastTimer);
      document.removeEventListener('visibilitychange', foreground);
      window.removeEventListener('napstrfy-media-action', handleSystemMediaAction);
      window.removeEventListener('keydown', handleKeyboard);
      timing.stop();
      mediaUpdates.cancel();
      clearMediaSession();
      androidMediaBridge()?.clear();
    };
  });
</script>

<svelte:head><title>Napstrfy</title></svelte:head>

{#if !status.paired && activeTab !== 'podcasts'}
  <main class="pair-screen">
    <div class="pair-glow"></div>
    <div class="pair-logo" aria-label="Napstrfy"><img src={appIcon} alt="" /><span>napstrfy</span></div>
    <p class="eyebrow">{$t("NAPSTR COMPANION")}</p>
    <h1>{$t("Your music.")}<br />{$t("Wherever you are.")}</h1>
    <p class="pair-copy">{$t("Connect to the computer running Napstr. Browse its library and listen here, with your music sent over an encrypted connection.")}</p>
    {#if error}
      <div class="error-card">
        <span>{$t(error)}</span>
        {#if cameraPermissionDenied}<button onclick={showCameraSettings}>{$t("Open app settings")}</button>{/if}
      </div>
    {/if}
    {#if mobile}
      <button class="scan-button" onclick={scanCode} disabled={scanning || pairing || statusLoading}><span>▦</span>{scanning ? $t("Opening camera…") : pairing ? $t("Pairing…") : $t("Scan Napstr QR")}</button>
    {/if}
    <details class="manual-pair" class:desktop-pair={!mobile} bind:open={manualPairOpen}>
      <summary>{mobile ? $t("Enter a pairing code instead") : $t("Connect with a pairing code")}</summary>
      <p>{$t("On the computer running Napstr, open")} <strong>{$t("Napstrfy → Pair without a camera")}</strong>{$t(". Copy the code and paste it here within five minutes.")}</p>
      <form onsubmit={(event) => { event.preventDefault(); void pair(); }}>
        <textarea bind:value={pairingCode} aria-label={$t("Napstr pairing code")} placeholder="napstrfy://pair/…" spellcheck="false" autocapitalize="off" autocomplete="off"></textarea>
        <button type="submit" disabled={!pairingCode.trim() || pairing || statusLoading}>{pairing ? $t("Connecting…") : $t("Connect to Napstr")}</button>
      </form>
    </details>
    <button class="browse-podcasts" onclick={showPodcasts}>{$t("Listen to podcasts without pairing")}</button>
    <LanguageSelect />
    <small class="pair-security">{$t("One-use pairing · no Nostr keys leave your computer")}</small>
  </main>
{:else}
  <main class="app-shell" class:desktop={!mobile && platform !== ''}>
    <header class="app-header">
      <div class="brand"><img src={appIcon} alt="" /><b>napstrfy</b></div>
      {#if status.paired}
        <button class="desktop-status" class:offline={!status.connected} onclick={reconnect}><i></i><span>{statusPending ? $t("Connecting…") : status.connected ? status.desktopName || $t("Napstr connected") : $t("Reconnect")}{status.streamOnly ? $t(" · Read only") : ''}</span></button>
      {:else}
        <button class="desktop-status offline" onclick={() => (activeTab = 'music')}><i></i><span>{$t("Pair Napstr for music")}</span></button>
      {/if}
    </header>

    <div class="app-content">
    {#if error}<button class="error-banner" onclick={() => (error = '')}>{$t(error)}<span>×</span></button>{/if}
    {#if notice}<button class="notice-banner" onclick={() => (notice = '')}>{$t(notice)}<span>×</span></button>{/if}

    {#if activeTab === 'music'}
      <section class="search-area">
        <form onsubmit={(event) => { event.preventDefault(); event.currentTarget.querySelector('input')?.blur(); void searchTracks(); }}>
          <span>⌕</span><input bind:value={query} placeholder={status.streamOnly ? $t("Search Napstr’s music") : $t("Search your music and Nostr")} aria-label={$t("Search tracks")} />
          {#if query}<button type="button" class="clear-search" onclick={() => searchTracks('')}>×</button>{/if}
        </form>
        <div class="chips"><button class:active={showingLikedMusic} onclick={showLikedTracks}>{$t("♥ Liked")}</button>{#each musicChips as chip}<button class:active={!showingLikedMusic && query.toLocaleLowerCase() === chip.toLocaleLowerCase()} onclick={() => selectChip(chip)}>{$t(chip)}</button>{/each}</div>
      </section>

      <section class="library-heading">
        <div><p>{showingLikedMusic ? $t("FAVOURITES") : query ? $t("SEARCH RESULTS") : $t("YOUR NAPSTR")}</p><h1>{showingLikedMusic ? $t("Liked music") : query ? query : $t("Your music")}</h1></div>
        <span>{$t("Tracks: {count}", { count: total })}</span>
      </section>

      <section class="track-list" aria-busy={loading || searchingNetwork}>
        {#if loading && tracks.length === 0}<div class="loading-list"><i></i><span>{$t("Asking Napstr…")}</span></div>{/if}
        {#if !loading && !searchingNetwork && tracks.length === 0}<div class="empty-library"><img src={appIcon} alt="" /><h2>{showingLikedMusic ? $t("No liked tracks yet") : $t("No tracks found")}</h2><p>{showingLikedMusic ? $t("Tap the heart beside a song to keep it here.") : query ? $t("Try different words or clear the search.") : $t("Add music to your Napstr folder on the computer.")}</p></div>{/if}
        {#each tracks as track (track.fileId)}
          <div class:selected={selected?.fileId === track.fileId} class:remote={!track.local} class="track-row">
            <button class="track-open" disabled={status.streamOnly && !track.local} onclick={() => activateTrack(track)}>
              <TrackArtwork {track} />
              <span class="track-copy">
                <strong>{title(track)}</strong>
                <small>{artist(track)}{track.album ? ` · ${track.album}` : ''}</small>
                <span class="track-meta">{readableSize(track.size)}{#if !track.local} · {track.sources.length} {track.sources.length === 1 ? $t("seeder") : $t("seeders")}{/if}</span>
              </span>
              <span class="track-action">{pending.has(track.fileId) ? '···' : track.local ? '⋮' : status.streamOnly ? $t("Unavailable") : '⇩'}</span>
            </button>
            <button class:liked={isTrackLiked(track)} class="like-button" onclick={() => toggleTrackLike(track)} aria-label={$t(isTrackLiked(track) ? 'Unlike {p0}' : 'Like {p0}', { p0: title(track) })}>{isTrackLiked(track) ? '♥' : '♡'}</button>
          </div>
        {/each}
        {#if searchingNetwork && (!loading || tracks.length > 0)}<div class="loading-list network-search-status" role="status"><i></i><span>{$t("Searching")}…</span></div>{/if}
        {#if !showingLikedMusic && tracks.length < total}<button class="load-more" onclick={() => loadLibrary(true)} disabled={loadingMore}>{loadingMore ? $t("Loading…") : $t("Load more · {p0} of {p1}", { p0: tracks.length, p1: total })}</button>{/if}
      </section>
    {:else if activeTab === 'podcasts'}
      <section class="search-area podcast-search">
        <form onsubmit={(event) => { event.preventDefault(); event.currentTarget.querySelector('input')?.blur(); void searchPodcasts(); }}>
          <span>⌕</span><input bind:value={podcastQuery} placeholder={$t("Search podcasts")} aria-label={$t("Search podcasts")} />
          {#if podcastQuery}<button type="button" class="clear-search" onclick={() => { podcastQuery = ''; void loadTrendingPodcasts(); }}>×</button>{/if}
        </form>
        <div class="chips podcast-genres"><button class:active={showingLikedPodcasts} onclick={showLikedPodcastList}>{$t("♥ Liked")}</button>{#each podcastGenres as genre}<button class:active={!showingLikedPodcasts && podcastGenre === genre} onclick={() => selectPodcastGenre(genre)}>{$t(genre)}</button>{/each}</div>
      </section>

      {#if selectedPodcast}
        <section class="podcast-show-heading">
          <button class="podcast-back" onclick={() => { selectedPodcast = null; podcastEpisodes = []; }}>‹</button>
          {#if selectedPodcast.image}<img src={selectedPodcast.image} alt="" />{:else}<div class="podcast-art-fallback">◉</div>{/if}
          <div><p>{$t("PODCAST")}</p><h1>{selectedPodcast.title}</h1><small>{selectedPodcast.author || $t("Independent podcast")}</small></div>
          <button class:liked={isPodcastLiked(selectedPodcast)} class="like-button podcast-heading-like" onclick={() => togglePodcastLike(selectedPodcast!)} aria-label={$t(isPodcastLiked(selectedPodcast) ? 'Unlike {p0}' : 'Like {p0}', { p0: selectedPodcast.title })}>{isPodcastLiked(selectedPodcast) ? '♥' : '♡'}</button>
        </section>
        <section class="episode-list" aria-busy={podcastLoading}>
          {#if podcastLoading}<div class="loading-list"><i></i><span>{$t("Loading episodes…")}</span></div>{/if}
          {#each podcastEpisodes as episode (episode.id)}
            {@const download = podcastDownloadFor(episode.id)}
            {@const episodeImage = episode.image || selectedPodcast.image}
            {@const downloadProgress = Math.max(0, Math.min(100, Math.round(download?.progress || 0)))}
            <article class="episode-row">
              <button class="episode-art" onclick={() => playPodcast(episode)} aria-label={$t("Play {p0}", { p0: episode.title })}>
                <span class="podcast-art-fallback">◉</span>
                {#if episodeImage}<img src={episodeImage} alt="" onerror={(event) => usePodcastArtwork(event, selectedPodcast!.image)} />{/if}
                <i aria-hidden="true">▶</i>
              </button>
              <button class="episode-copy" onclick={() => playPodcast(episode)}>
                <strong>{episode.title}</strong>
                {#if episode.description}<span>{episode.description}</span>{/if}
                <small>{podcastDate(episode.datePublished)}{episode.duration ? ` · ${clock(episode.duration)}` : ''}</small>
              </button>
              {#if download?.status === 'Downloading' && !download.ready}
                <div class="episode-download downloading" role="progressbar" aria-label={$t("Download {p0}", { p0: episode.title })} aria-valuemin="0" aria-valuemax="100" aria-valuenow={downloadProgress} aria-valuetext={`${$t("Downloading…")} ${downloadProgress}%`}>
                  <svg class="episode-download-ring" viewBox="0 0 36 36" aria-hidden="true">
                    <circle class="episode-download-track" cx="18" cy="18" r="15" />
                    <circle class="episode-download-progress" cx="18" cy="18" r="15" pathLength="100" stroke-dasharray="100" stroke-dashoffset={100 - downloadProgress} transform="rotate(-90 18 18)" />
                  </svg>
                  <span class="episode-download-label" aria-hidden="true">{downloadProgress}%</span>
                </div>
              {:else}
                <button class:ready={download?.ready} class="episode-download" onclick={() => downloadPodcast(episode)} aria-label={download?.ready ? `${$t("Ready to play")}: ${episode.title}` : $t("Download {p0}", { p0: episode.title })} title={download?.status || $t("Download for offline listening")}>
                  {#if download?.ready}<span class="episode-download-complete" aria-hidden="true">✓</span>{:else}⇩{/if}
                </button>
              {/if}
              {#if download?.status.startsWith('Failed:')}
                <p class="episode-download-error" role="alert">{$t(msg("Could not download {p0}: {p1}", { p0: episode.title, p1: download.status.slice('Failed:'.length).trim() }))}</p>
              {/if}
            </article>
          {/each}
          {#if !podcastLoading && podcastEpisodes.length === 0}<div class="empty-library"><h2>{$t("No playable episodes")}</h2><p>{$t("This feed may not currently expose supported HTTPS audio.")}</p></div>{/if}
        </section>
      {:else}
        {#if podcastHistory.length > 0 && !podcastQuery && !podcastGenre && !showingLikedPodcasts}
          <section class="podcast-history"><div class="section-label"><b>{$t("Recently played")}</b><span>{$t("Last 10")}</span></div><div class="history-scroller">{#each podcastHistory as episode (episode.id)}<button onclick={() => playPodcast(episode)}>{#if episode.image}<img src={episode.image} alt="" />{:else}<span>◉</span>{/if}<strong>{episode.title}</strong><small>{episode.feedTitle}</small></button>{/each}</div></section>
        {/if}
        <section class="library-heading">
          <div><p>{showingLikedPodcasts ? $t("FAVOURITES") : $t("POWERED BY PODCAST INDEX")}</p><h1>{showingLikedPodcasts ? $t("Liked podcasts") : podcastGenre ? podcastGenre : podcastQuery ? $t("Results for “{p0}”", { p0: podcastQuery }) : $t("Discover podcasts")}</h1></div>
          <span>{podcastFeeds.length} {$t("shows")}</span>
        </section>
        <section class="podcast-grid" aria-busy={podcastLoading}>
          {#if podcastLoading}<div class="loading-list"><i></i><span>{$t("Searching podcasts…")}</span></div>{/if}
          {#each podcastFeeds as feed (feed.id)}
            <article class="podcast-card">
              <button class="podcast-open" onclick={() => openPodcast(feed)}>
                {#if feed.image}<img src={feed.image} alt="" />{:else}<div class="podcast-art-fallback">◉</div>{/if}
                <span><strong>{feed.title}</strong><small>{feed.author || $t("Independent podcast")}</small></span>
              </button>
              <button class:liked={isPodcastLiked(feed)} class="like-button podcast-like" onclick={() => togglePodcastLike(feed)} aria-label={$t(isPodcastLiked(feed) ? 'Unlike {p0}' : 'Like {p0}', { p0: feed.title })}>{isPodcastLiked(feed) ? '♥' : '♡'}</button>
            </article>
          {/each}
          {#if !podcastLoading && podcastFeeds.length === 0}<div class="empty-library"><h2>{showingLikedPodcasts ? $t("No liked podcasts yet") : $t("Search podcasts")}</h2><p>{showingLikedPodcasts ? $t("Use the heart beside a podcast to keep it here.") : $t("Napstrfy searches podcasts directly over this device’s internet connection.")}</p></div>{/if}
        </section>
      {/if}
    {:else}
      <section class="search-area audiobook-search">
        <form onsubmit={(event) => { event.preventDefault(); event.currentTarget.querySelector('input')?.blur(); void loadAudiobooks(); }}>
          <span>⌕</span><input bind:value={audiobookQuery} placeholder={$t("Search audiobooks")} aria-label={$t("Search audiobooks")} />
          {#if audiobookQuery}<button type="button" class="clear-search" onclick={() => { audiobookQuery = ''; void loadAudiobooks(); }}>×</button>{/if}
        </form>
      </section>

      {#if selectedAudiobook}
        <section class="audiobook-show-heading">
          <button class="podcast-back" onclick={() => (selectedAudiobook = null)}>‹</button>
          <div class="audiobook-cover">▥</div>
          <div><p>{$t("AUDIOBOOK")}</p><h1>{selectedAudiobook.title}</h1><small>{selectedAudiobook.author || $t("Unknown author")}{selectedAudiobook.narrator ? $t(" · Read by {p0}", { p0: selectedAudiobook.narrator }) : ''}</small></div>
        </section>
        <section class="audiobook-chapter-list" aria-busy={audiobookLoading}>
          {#each selectedAudiobook.chapters as chapter, index (chapter.fileId)}
            <button class="audiobook-chapter" disabled={status.streamOnly && !chapter.local} onclick={() => activateAudiobookChapter(selectedAudiobook!, chapter)}>
              <span>{chapter.local ? '▶' : status.streamOnly ? '—' : '⇩'}</span>
              <span><strong>{chapter.title || chapter.filename}</strong><small>{$t("Chapter")} {index + 1} · {readableSize(chapter.size)}</small></span>
            </button>
          {/each}
        </section>
      {:else}
        <section class="library-heading">
          <div><p>{$t("YOUR NAPSTR")}</p><h1>{$t("Audiobooks")}</h1></div>
          <span>{$t("Audiobooks: {count}", { count: audiobookTotal })}</span>
        </section>
        <section class="audiobook-list" aria-busy={audiobookLoading}>
          {#if audiobookLoading}<div class="loading-list"><i></i><span>{$t("Asking Napstr…")}</span></div>{/if}
          {#each audiobooks as book (book.audiobookId)}
            <button class="audiobook-card" onclick={() => openAudiobook(book)}>
              <span class="audiobook-cover">▥</span>
              <span><strong>{book.title}</strong><small>{book.author || $t("Unknown author")}</small><i>{book.chapterCount} {book.chapterCount === 1 ? $t("file") : $t("chapters")} · {readableSize(book.totalSize)}</i></span>
              <b>›</b>
            </button>
          {/each}
          {#if !audiobookLoading && audiobooks.length === 0}<div class="empty-library"><h2>{$t("No audiobooks found")}</h2><p>{$t("Group a chapter folder in Napstr, or add the tag “audiobook” to a complete one-file book.")}</p></div>{/if}
        </section>
      {/if}
    {/if}

    </div>

    <nav class="app-nav" aria-label={$t("Napstrfy navigation")}>
      <button class:active={activeTab === 'music'} aria-current={activeTab === 'music' ? 'page' : undefined} onclick={() => (activeTab = 'music')}><span aria-hidden="true">♫</span>{$t("Music")}</button>
      <button class:active={activeTab === 'podcasts'} aria-current={activeTab === 'podcasts' ? 'page' : undefined} onclick={showPodcasts}><span aria-hidden="true">◉</span>{$t("Podcasts")}</button>
      <button class:active={activeTab === 'audiobooks'} aria-current={activeTab === 'audiobooks' ? 'page' : undefined} onclick={showAudiobooks}><span aria-hidden="true">▥</span>{$t("Audiobooks")}</button>
      <button onclick={showPairing}><span aria-hidden="true">⚙</span>{$t("Settings")}</button>
    </nav>

    <section class:empty={activeMedia === 'music' ? !current : !currentPodcast} class="now-playing">
      <div class="player-backdrop" aria-hidden="true"><span style:background-image={playerArtwork ? `url(${JSON.stringify(playerArtwork)})` : undefined}></span></div>
      <div class="player-content">
      {#if activeMedia === 'podcast' && currentPodcast}
        {#if currentPodcast.image}<img class="podcast-player-art" src={currentPodcast.image} alt="" />{:else}<div class="empty-art">◉</div>{/if}
      {:else if current}<TrackArtwork track={current} large lookup onartworkchange={(url) => { currentArtwork = url; }} />{:else}<div class="empty-art">♪</div>{/if}
      <div class="now-copy"><strong>{activeMedia === 'podcast' && currentPodcast ? currentPodcast.title : current ? title(current) : $t("Choose something to play")}</strong><small>{activeMedia === 'podcast' && currentPodcast ? currentPodcast.feedTitle : current ? artist(current) : $t("Music and podcasts, wherever you are")}</small></div>
      <div class="timeline"><input aria-label={$t("Playback position")} type="range" min="0" max={displayedDuration || 0} step="0.1" value={currentTime} oninput={(event) => seek(Number(event.currentTarget.value))} disabled={!canSeek} /><span>{clock(currentTime)} / {displayedDuration ? `${duration ? '' : '≈ '}${clock(displayedDuration)}` : '—'}</span></div>
      <div class="player-buttons">
        <button onclick={() => moveTrack(-1)} disabled={activeMedia !== 'music' || playerQueue.length < 2 || (playMode === 'random' && randomHistoryIndex <= 0)} aria-label={$t("Previous track")}>|◀</button>
        <button class="seek-button" onclick={() => skipSeconds(-15)} disabled={!canSeek} aria-label={$t("Back 15 seconds")} title={$t("Back 15 seconds")}><SeekIcon /></button>
        <button class="play-main" onclick={togglePlayer} aria-label={caching ? $t("Loading audio") : playing ? $t("Pause") : $t("Play")} title={$t("Play / pause (Space)")} disabled={(!current && !currentPodcast) || caching}>{caching ? '···' : playing ? 'Ⅱ' : '▶'}</button>
        <button class="seek-button" onclick={() => skipSeconds(15)} disabled={!canSeek} aria-label={$t("Forward 15 seconds")} title={$t("Forward 15 seconds")}><SeekIcon forward /></button>
        <button onclick={() => moveTrack(1)} disabled={activeMedia !== 'music' || playerQueue.length < 2} aria-label={$t("Next track")}>▶|</button>
        <button class="like-button player-like" class:liked={playerLiked} disabled={!playerLikeTitle}
          onclick={() => { if (activeMedia === 'music' && current) toggleTrackLike(current); else if (currentPodcastFeed) togglePodcastLike(currentPodcastFeed); }}
          aria-pressed={playerLiked} aria-label={$t(playerLiked ? 'Unlike {p0}' : 'Like {p0}', { p0: playerLikeTitle || '' })}
          title={$t(playerLiked ? 'Unlike {p0}' : 'Like {p0}', { p0: playerLikeTitle || '' })}>{playerLiked ? '♥' : '♡'}</button>
        <button class="mode-button" class:active={activeMedia === 'music'} onclick={cyclePlayMode} disabled={activeMedia !== 'music'} aria-label={$t(playModeDetails().label)} title={$t(playModeDetails().label)}><span aria-hidden="true">{playModeDetails().icon}</span><span class="mode-label">{$t(playModeDetails().label)}</span></button>
      </div>
      <label class="volume">{$t("Volume")} <input aria-label={$t("Volume")} type="range" min="0" max="1" step="0.02" value={volume} oninput={(event) => setVolume(Number(event.currentTarget.value))} /></label>
      </div>
    </section>
  </main>
{/if}

<dialog bind:this={pairingDialog} class="pairing-dialog" aria-labelledby="pairing-heading">
  <h2 id="pairing-heading">{$t("Settings")}</h2>
  {#if status.paired}
  <LanguageSelect />
  <h3>{$t("Your Napstr connection")}</h3>
  <p class="connected-name">{status.desktopName || 'Napstr'}</p>
  <p>{status.connected ? $t("Connected") : $t("Offline")} · {status.streamOnly ? $t("Read-only access") : $t("Full access")}</p>
  <p>{$t("Napstr must stay running to browse and fetch audio. Cached music can play offline.")}</p>
  <p>{$t("Disconnecting will require a new pairing code to connect again.")}</p>
  {/if}
  <div class="dialog-actions">{#if status.paired}<button class="disconnect-button" onclick={forgetDesktop}>{$t("Disconnect")}</button>{/if}<button onclick={() => pairingDialog.close()}>{$t("Done")}</button></div>
</dialog>

<audio
  bind:this={audio}
  onplay={() => { playing = true; syncSystemMedia(); }}
  onpause={() => { playing = false; syncSystemMedia(); }}
  ontimeupdate={updatePlaybackPosition}
  onloadedmetadata={() => timing.request()}
  ondurationchange={() => timing.request()}
  onended={handleTrackEnded}
  onerror={() => {
    if (activeMedia === 'podcast' && currentPodcast) error = msg("This device could not play {p0}.", { p0: currentPodcast.title });
    else if (current) error = msg("This device could not decode {p0} audio.", { p0: current.format });
  }}
></audio>
