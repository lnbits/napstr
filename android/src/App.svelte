<script lang="ts">
  import { t, locale, message as msg, initializeLocale, type Message } from '@napstr/i18n/svelte';
  import LanguageSelect from '@napstr/i18n/LanguageSelect.svelte';
  import { locale as osLocale } from '@tauri-apps/plugin-os';
  import '@napstr/i18n/styles.css';
  import { onMount, tick, untrack } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { getCurrent, onOpenUrl } from '@tauri-apps/plugin-deep-link';
  import { parseDeepLink } from './lib/deepLink';
  import {
    Format,
    checkPermissions,
    openAppSettings,
    requestPermissions,
    scan
  } from '@tauri-apps/plugin-barcode-scanner';
  import TrackArtwork from './lib/TrackArtwork.svelte';
  import TrackBadge from './lib/TrackBadge.svelte';
  import CoverDebug from './lib/CoverDebug.svelte';
  import SeekIcon from './lib/SeekIcon.svelte';
  import { rateLimitedTask, safePosition, validDuration } from './lib/playback';
  import appIcon from '../src-tauri/icons/icon.png';
  import { artworkHue, coverFor, coverKey, invalidateCoverNegatives, loadFullCover, preloadArtwork, type AlbumCover } from './lib/artwork';
  import { reportReasons } from './lib/types';
  import type { AudiobookLibraryPage, CachedAudio, CompanionStatus, CoverReport, LibraryPage, PlaybackCommand, PodcastDownload, PodcastEpisode, PodcastFeed, ReadOnlyTicketOffer, RemoteAudiobook, RemoteAudiobookSummary, RemotePlaybackState, RemoteRepeat, RemoteTrack, RemoteTransfer, ReportReason } from './lib/types';

  const musicChips = ['Rock', 'Soundtrack', 'Punk', 'Folk', 'Upbeat'];
  const musicHistoryKey = 'napstrfy-played-albums';
  /** Matches the CSS transition, so the drawer unmounts once it has slid away. */
  const SHEET_ANIMATION_MS = 280;
  /** Fraction of the drawer's height a drag must cover to dismiss it. */
  const SHEET_DISMISS_RATIO = 0.2;
  /** Fraction of the screen a drag up on the collapsed bar must cover to open it. */
  const BAR_OPEN_RATIO = 0.2;
  /** Upward speed, in px/ms, that opens the drawer even on a short pull. */
  const BAR_FLING_SPEED = 0.35;
  /** Movement below this is a tap on the bar, not a pull. */
  const BAR_DRAG_SLOP = 6;
  /** How long a notice stays up. Its fade-out animation (`.toast`) ends just
   *  before this, so the element is unmounted after it has already gone. */
  const NOTICE_VISIBLE_MS = 4200;
  /** Temporary: cover-art diagnostics overlay. Delete with CoverDebug.svelte. */
  const COVER_DEBUG = true;
  /** The host caps a library page at 200, so one album always fits. */
  const MAX_ALBUM_TRACKS = 200;
  /** Fraction of the screen a right swipe on the liked page must cover to leave it. */
  const LIKED_SWIPE_DISMISS_RATIO = 0.25;
  /** Albums grouped out of the tracks this phone has loaded. */
  type AlbumShelf = {
    key: string;
    artist: string;
    album: string;
    representative: RemoteTrack;
    tracks: RemoteTrack[];
  };
  type ArtistShelf = { name: string; representative: RemoteTrack; count: number };
  type PlayedAlbum = { key: string; artist: string; album: string };
  const podcastGenres = ['Comedy', 'News', 'True Crime', 'Society & Culture', 'Technology', 'History', 'Business', 'Science', 'Arts', 'Sports', 'Education', 'Music'];
  const likedMusicKey = 'napstrfy-liked-music';
  const likedPodcastsKey = 'napstrfy-liked-podcasts';
  type AppTab = 'music' | 'search' | 'podcasts' | 'audiobooks';
  /** Repeating is a choice of three, and shuffling is independent of it. */
  type LoopMode = 'off' | 'all' | 'one';
  const LOOP_MODES: LoopMode[] = ['off', 'all', 'one'];
  /** Mirrors `MAX_PLAY_QUEUE` on the host: one request carries the whole list. */
  const MAX_DESKTOP_QUEUE = 200;
  const LOOP_LABELS: Record<LoopMode, string> = {
    off: 'Repeat off',
    all: 'Repeat all',
    one: 'Repeat this track'
  };
  const playModeKey = 'napstrfy-play-mode';
  type SleepOption = { value: string; label: string; minutes?: number; endsTrack?: boolean };
  const SLEEP_OPTIONS: SleepOption[] = [
    { value: '5', label: '5 minutes', minutes: 5 },
    { value: '10', label: '10 minutes', minutes: 10 },
    { value: '15', label: '15 minutes', minutes: 15 },
    { value: '30', label: '30 minutes', minutes: 30 },
    { value: '45', label: '45 minutes', minutes: 45 },
    { value: '60', label: '1 hour', minutes: 60 },
    { value: 'track', label: 'End of track', endsTrack: true }
  ];
  /** Everything the album view needs once its tracks have been gathered. */
  type AlbumView = {
    key: string;
    artist: string;
    album: string;
    year: string;
    art: string;
    thumb: string;
    tracks: RemoteTrack[];
    more: AlbumShelf[];
  };
  let activeTab = $state<AppTab>('music');
  /** What Tauri reports this build is: a phone, or one of the desktop systems. */
  let platform = $state('');
  const mobile = $derived(platform === 'android' || platform === 'ios');
  /**
   * The three-column window. The album preview is a sibling of the shell rather
   * than a child of it, so both elements need this flag, and it is derived once
   * so the two can never disagree about when the columns exist.
   */
  const desktopShell = $derived(!mobile && platform !== '');
  /**
   * A desktop window wide enough for three columns pins the now-playing sheet as
   * the third one, instead of leaving it as a drawer over the content. A phone
   * never pins, whatever its width: the sheet there is the full-screen drawer.
   */
  let wideWindow = $state(false);
  $effect(() => {
    if (mobile || platform === '') return;
    const query = window.matchMedia('(min-width: 800px)');
    wideWindow = query.matches;
    const listener = (event: MediaQueryListEvent) => { wideWindow = event.matches; };
    query.addEventListener('change', listener);
    return () => query.removeEventListener('change', listener);
  });
  const pinned = $derived(!mobile && platform !== '' && wideWindow);
  let status = $state<CompanionStatus>({ streamOnly: false, paired: false, connected: false, desktopName: '', endpointId: '', libraryRevision: 0, coverRevision: 0, error: '' });
  let statusLoading = $state(true);
  let statusPending = $state(false);
  let pairingCode = $state('');
  let pairing = $state(false);
  let scanning = $state(false);
  let cameraPermissionDenied = $state(false);
  /** Open on a desktop, where there is no camera to scan with. */
  let manualPairOpen = $state(true);
  $effect(() => { manualPairOpen = !mobile; });
  // Either a finished sentence, or a `msg(...)` still to be translated, so a
  // notice already on screen follows a change of language.
  let error = $state<string | Message>('');
  let notice = $state<string | Message>('');
  let query = $state('');
  let tracks = $state<RemoteTrack[]>([]);
  let likedMusic = $state<RemoteTrack[]>([]);
  let showingLikedMusic = $state(false);
  let total = $state(0);
  let loading = $state(false);
  /** The network half of a search is still running while the host's half is not. */
  let searchingNetwork = $state(false);
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
  let loopMode = $state<LoopMode>('all');
  let shuffle = $state(false);
  let randomHistory = $state<number[]>([]);
  let randomHistoryIndex = $state(-1);
  let randomUpcoming = $state(-1);
  let playing = $state(false);
  let caching = $state(false);
  let currentTime = $state(0);
  let duration = $state(0);
  /**
   * True while `duration` is the podcast feed's own claim rather than a length
   * the audio has reported. A feed can be wrong, so a hint may be shown with a
   * "~" but must never enable seeking or be published to the system controls.
   */
  let durationEstimated = $state(false);
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
  // The expandable now-playing sheet. Audiobook chapters run through the same
  // player as music, so the library flag is what distinguishes "music only".
  let showNowPlaying = $state(false);
  let nowCover = $state<AlbumCover | null>(null);
  let nowArtFailed = $state(false);
  /** The sheet's full-cover URL once that image has landed, so the small
   *  rendition under it stays on screen until something better is drawn. */
  let sheetArtLoaded = $state('');
  /**
   * The best cover the system has been given for what is playing: the thumbnail
   * to begin with, then the full cover once that one has landed.
   */
  let lockScreenCover = $state('');
  /** The track the resolved cover belongs to, so a new object for the same one
   *  does not throw the artwork away and fetch it again. */
  let nowCoverKey = '';
  let sheetDragY = $state(0);
  let sheetDragging = $state(false);
  let sheetDragPending = $state(false);
  /** False when the gesture began inside a queue that is scrolled down. */
  let sheetDragAllowed = false;
  let sheetDragStart = 0;
  /** Scroll offset when the gesture began: a scrolled list scrolls, it does not drag. */
  let sheetScrollTop = 0;
  /** Set for one frame on open so the drawer slides up instead of appearing. */
  let sheetEntering = $state(false);
  /** Set while sliding away; the drawer unmounts when the animation ends. */
  let sheetClosing = $state(false);
  /** The playlist lives in its own view so the drawer never scrolls. */
  let showQueue = $state(false);
  /** The track menu, opened from the drawer or from any album track. */
  let showActions = $state(false);
  let showSleepOptions = $state(false);
  let actionTrack = $state<RemoteTrack | null>(null);
  let sleepValue = $state('');
  let sleepEndsAt = $state(0);
  let sleepClock = $state('');
  /** Album preview, split into distinct sections. */
  let showAlbumView = $state(false);
  let albumView = $state<AlbumView | null>(null);
  /** The header `art` URL whose full image has landed, so the small rendition
   *  it is standing on can stay there until something better is on screen. */
  let albumArtLoaded = $state('');
  /** The header backdrop: the small rendition, which is all a blurred glow can
   *  show and the one the shelf tile has already fetched. */
  let albumGlow = $derived(albumView ? albumView.thumb || albumView.art : '');
  /** File ids held in this phone's audio cache, for the storage badge. */
  let cachedFileIds = $state<Set<string>>(new Set());
  let showSettings = $state(false);
  /** The computer's player, drawn by the same drawer as this phone's. */
  let remoteState = $state<RemotePlaybackState | null>(null);
  let remoteBusy = $state(false);
  let remoteError = $state('');
  /**
   * The list this phone sent the last time it asked the computer to play. The
   * wire carries only the queue's length and position, so this is a best-effort
   * copy of what is queued over there: it is what the drawer draws, and its
   * playing row is only marked while its length still matches what the computer
   * reports.
   */
  let remoteQueue = $state<RemoteTrack[]>([]);
  /**
   * The computer's volume while it is being changed from here. -1 means "ask the
   * computer"; a burst of volume-key presses accumulates in here so that holding
   * the key becomes one request rather than one per key event.
   */
  let remoteVolume = $state(-1);
  let remoteVolumeTimer = 0;
  /**
   * The track a sleep timer set to "end of track" is waiting on, when the
   * computer is the one playing. The phone cannot hear a remote track end, so a
   * different file being reported is what counts as the end.
   */
  let sleepRemoteFileId = '';
  /**
   * The computer's position is only known when it answers, so between answers it
   * is carried forward from here. This tick is what makes the bar move.
   */
  let remoteTick = $state(0);
  /** When the last answer from the computer arrived, on this phone's clock. */
  let remoteStateAt = 0;
  let remoteFetching = false;
  /** The track the computer has already been asked about at its own end. */
  let remoteEndedFileId = '';
  /** The devices a tap can be sent to. Bluetooth outputs will join this list. */
  type PlaybackTarget = 'phone' | 'desktop';
  /**
   * Where a tap plays. Deliberately not remembered across launches: a phone that
   * quietly plays to a computer somebody else is sitting at is a surprise, and
   * choosing the source again costs one tap.
   */
  let playbackTarget = $state<PlaybackTarget>('phone');
  let showSourceOptions = $state(false);
  /** The track's own code, drawn when that row of the track menu is chosen. */
  let showTrackCode = $state(false);
  let trackCodeSvg = $state('');
  let trackCodeError = $state('');
  /** A read-only code the computer minted for somebody else to scan. */
  let readOnlyTicket = $state<ReadOnlyTicketOffer | null>(null);
  let ticketBusy = $state(false);
  let ticketError = $state('');
  /** NIP-56: reporting the cover on an album, opened from the track menu. */
  let showReport = $state(false);
  let reportKey = $state('');
  let reportLabel = $state('');
  let reportReason = $state<ReportReason>('spam');
  let reportNote = $state('');
  let reportBusy = $state(false);
  let reportError = $state('');
  /** The album view's scroller, so opening another album can jump to the top. */
  let albumScroll = $state<HTMLDivElement | undefined>(undefined);

  $effect(() => {
    // A different album means a fresh page, not the last one's scroll position.
    const key = albumView?.key;
    const element = albumScroll;
    if (key && element) element.scrollTop = 0;
  });
  let sheetElement = $state<HTMLDivElement | undefined>(undefined);
  let sheetScroller = $state<HTMLDivElement | undefined>(undefined);
  let sheetCloseTimer = 0;
  /** Dragging the collapsed bar upward pulls the drawer into view. */
  let barElement = $state<HTMLButtonElement | undefined>(undefined);
  let barDragging = $state(false);
  let barDragTravelled = $state(0);
  let barDragStart = 0;
  let barDragLastY = 0;
  let barDragLastAt = 0;
  let barDragSpeed = 0;
  /** Where the bar rests: the drawer's top edge starts level with it. */
  let barRestTop = $state(0);
  let barSwallowClick = false;

  /** How far the finger must pull to open, which is also how far the bar fades. */
  let barOpenTravel = $derived(Math.max(1, barRestTop * BAR_OPEN_RATIO));
  /** The bar rides up with the drawer and is gone by the time it would open. */
  let barShift = $derived(barDragging ? -barDragTravelled : 0);
  let barFade = $derived(barDragging ? Math.max(0, 1 - barDragTravelled / barOpenTravel) : 1);
  /** The played portion of the card, starting where the artwork ends. */
  let barProgress = $derived(
    playbackTarget === 'desktop'
      ? remoteState && remoteState.durationMs > 0
        ? Math.min(1, Math.max(0, remotePositionMs() / remoteState.durationMs))
        : 0
      : duration > 0
        ? Math.min(1, Math.max(0, currentTime / duration))
        : 0
  );
  /** The track the bar draws, which is the computer's when it is the source. */
  let barTrack = $derived(playbackTarget === 'desktop' ? desktopTrackFromState() : null);
  /** The cover the bar stretches behind itself, or '' when there is none. It is
   *  blurred far past what a large image could show, so it takes the small
   *  rendition: the one the tile that started this track has already fetched. */
  let barArtwork = $derived(
    playbackTarget === 'desktop'
      ? sheetThumbUrl() || sheetCoverUrl()
      : activeMedia === 'podcast'
        ? currentPodcast?.image ?? ''
        : sheetThumbUrl() || sheetCoverUrl()
  );
  /** The custom properties its stretched cover needs, inert when there is none. */
  let barArtStyle = $derived(
    barArtwork ? `--bar-art:url(${barArtwork}); --bar-scrim:1` : '--bar-art:none; --bar-scrim:0'
  );
  /** True when the bar has nothing to show, whichever player it is showing. */
  let barEmpty = $derived(
    playbackTarget === 'desktop'
      ? !remoteState?.active
      : activeMedia === 'music'
        ? !current
        : !currentPodcast
  );
  let barPlaying = $derived(playbackTarget === 'desktop' ? remoteState?.playing === true : playing);
  /**
   * The player the bar, the drawer and the system's media controls are all
   * describing. They share one set of values so the three cannot disagree about
   * what is playing or how far into it they are.
   */
  let shownTrack = $derived(playbackTarget === 'desktop' ? desktopTrackFromState() : current);
  let shownPlaying = $derived(playbackTarget === 'desktop' ? remoteState?.playing === true : playing);
  let shownPosition = $derived(playbackTarget === 'desktop' ? remotePositionMs() / 1000 : currentTime);
  let shownDuration = $derived(playbackTarget === 'desktop' ? (remoteState?.durationMs ?? 0) / 1000 : duration);
  /** The length the audio itself reported: what can honestly be sought and published. */
  let verifiedDuration = $derived(playbackTarget === 'desktop' ? shownDuration : durationEstimated ? 0 : duration);
  /** An unknown length says so, and a feed's claim is marked as the estimate it is. */
  let shownDurationLabel = $derived(
    shownDuration > 0 ? `${playbackTarget === 'desktop' ? '' : durationEstimated ? '≈ ' : ''}${clock(shownDuration)}` : '—'
  );
  /** Whether the player on screen has a neighbour to move to. */
  let shownCanSkip = $derived(
    playbackTarget === 'desktop' ? (remoteState?.queueLen ?? 0) > 1 : playerQueue.length > 1
  );
  /**
   * Whether ±15 s can land anywhere: there has to be a live player on the
   * chosen side and a known length to clamp against. The timeline uses the same
   * rule, so the buttons and the bar never disagree about what is seekable.
   */
  let shownCanSeek = $derived(
    playbackTarget === 'desktop'
      ? !status.streamOnly && remoteState?.active === true
      : !caching && verifiedDuration > 0
  );
  /** The playlist the drawer would open: this phone's, or the copy of theirs. */
  let shownQueue = $derived(playbackTarget === 'desktop' ? remoteQueue : playerQueue);
  /** The row the playlist marks as playing, or -1 when that cannot be trusted. */
  let shownQueueIndex = $derived(
    playbackTarget === 'desktop'
      ? remoteQueue.length === (remoteState?.queueLen ?? 0)
        ? remoteState?.queueIndex ?? -1
        : -1
      : playerIndex
  );
  let shownShuffle = $derived(playbackTarget === 'desktop' ? remoteState?.shuffle === true : shuffle);
  let shownLoopActive = $derived(
    playbackTarget === 'desktop' ? (remoteState?.repeat ?? 'off') !== 'off' : loopMode !== 'off'
  );
  let shownLoopLabel = $derived(
    playbackTarget === 'desktop' ? `Repeat: ${remoteRepeatLabel(remoteState?.repeat ?? 'off')}` : LOOP_LABELS[loopMode]
  );
  let shownLiked = $derived(shownTrack ? isTrackLiked(shownTrack) : false);
  let shownLoopOne = $derived(playbackTarget === 'desktop' ? remoteState?.repeat === 'one' : loopMode === 'one');
  /**
   * Whichever track the open menu applies to: the chosen one, else the one the
   * drawer is showing. With the computer as the source that is its track, so the
   * menu still has a subject before this phone has played anything.
   */
  let menuTrack = $derived(
    actionTrack ??
      (playbackTarget === 'desktop'
        ? desktopTrackFromState()
        : activeMedia === 'music'
          ? current
          : null)
  );
  let nowTitle = $derived(
    playbackTarget === 'desktop'
      ? remoteState?.title || (remoteState?.active ? 'Unknown track' : 'Nothing is playing there')
      : activeMedia === 'podcast' && currentPodcast
        ? currentPodcast.title
        : current
          ? title(current)
          : 'Choose something to play'
  );
  let nowArtist = $derived(
    playbackTarget === 'desktop'
      ? remoteState?.artist || (remoteState?.active ? status.desktopName || 'The computer' : 'Pick something to play there')
      : activeMedia === 'podcast' && currentPodcast
        ? currentPodcast.feedTitle
        : current
          ? artist(current)
          : 'Music and podcasts, wherever you are'
  );
  /** A title wider than the card scrolls rather than being cut in half. */
  let titleClipper = $state<HTMLDivElement | undefined>(undefined);
  let titleText = $state<HTMLSpanElement | undefined>(undefined);
  let titleOverflows = $state(false);

  $effect(() => {
    const text = nowTitle;
    if (!text) {
      titleOverflows = false;
      return;
    }
    const clipper = titleClipper;
    const element = titleText;
    if (!clipper || !element) return;
    // Measure once the new title has been laid out.
    const frame = window.requestAnimationFrame(() => {
      titleOverflows = element.scrollWidth > clipper.clientWidth + 1;
    });
    return () => window.cancelAnimationFrame(frame);
  });
  /** The navigation bar slides away with the drawer instead of being covered. */
  let navShift = $derived(
    showNowPlaying ? Math.min(1, Math.max(0, 1 - sheetDragY / Math.max(1, barRestTop))) : 0
  );
  let discoverAlbums = $state<AlbumShelf[]>([]);
  let discoverSeed = '';
  let playedAlbums = $state<PlayedAlbum[]>(readPlayedAlbums());
  let audio: HTMLAudioElement;
  /**
   * The lock screen needs a position about once a second while the events that
   * carry one arrive several times a second, so they are coalesced into one
   * update rather than each calling into the native side.
   */
  const mediaUpdates = rateLimitedTask(() => publishSystemMedia());
  /**
   * A desktop window has no Android bridge, so the system's media controls read
   * the web Media Session instead. These remember what it was last told, so a
   * position that has not moved is not published again.
   */
  let lastSessionPosition = '';
  let lastSessionMetadata = '';
  let lastSessionState = '';

  /** What the system's media controls need to know, whichever surface draws them. */
  type SystemMediaState = {
    title: string;
    artist: string;
    playing: boolean;
    position: number;
    duration: number;
    artwork?: string;
    canPrevious?: boolean;
    canNext?: boolean;
    canSeek?: boolean;
    labels?: Record<string, string>;
    liked?: boolean;
    looping?: boolean;
    volume?: number;
    remote?: boolean;
  };
  /** True while the system's media controls have been told to show nothing. */
  let systemMediaEmpty = false;

  type AndroidMediaBridge = {
    update(payload: string): void;
    clear(): void;
  };

  function androidMediaBridge(): AndroidMediaBridge | undefined {
    return (window as Window & { NapstrfyMedia?: AndroidMediaBridge }).NapstrfyMedia;
  }

  type AndroidBackBridge = {
    /** True while the page has something for a back press to close. */
    setBackAvailable?(available: boolean): void;
    /** The older name for the same flag, for a page paired with an older app. */
    setDrawerOpen?(open: boolean): void;
  };

  function androidBackBridge(): AndroidBackBridge | undefined {
    return (window as Window & { NapstrfyBack?: AndroidBackBridge }).NapstrfyBack;
  }

  /**
   * Tell the native side whether back has anywhere to go.
   *
   * Kotlin cannot ask the page synchronously, so the flag is pushed on every
   * transition: while it is set the press is handed to the page, and while it is
   * clear the system takes it, so back leaves the app when nothing is open.
   */
  function pushBackAvailability(available: boolean) {
    const bridge = androidBackBridge();
    if (!bridge) return;
    if (bridge.setBackAvailable) bridge.setBackAvailable(available);
    else bridge.setDrawerOpen?.(available);
  }

  /** The hardware back button arrives as an event, not a callback. */
  function handleSystemBack() {
    // Android took the flag to hand us this press, so the answer below has to be
    // published again: the states that follow this one are not always a change
    // of answer, and a derived value that stays true would publish nothing.
    backPresses += 1;
    if (showReport) {
      showReport = false;
      return;
    }
    // The source picker standing on its own, rather than inside the track menu.
    if (showSourceOptions && !showActions) {
      showSourceOptions = false;
      return;
    }
    if (showSettings) {
      showSettings = false;
      return;
    }
    if (showActions) {
      closeActions();
      return;
    }
    if (showAlbumView) {
      closeAlbumView();
      return;
    }
    if (showQueue) {
      showQueue = false;
      return;
    }
    if (showNowPlaying) {
      closeNowPlaying();
      return;
    }
    // The liked page is a page of its own, so back leaves it exactly as its own
    // close button does: for the search page it was opened from.
    if (showingLikedMusic) {
      closeLikedMusic();
      return;
    }
    // Back from any other tab is the way home.
    if (activeTab !== 'music') activeTab = 'music';
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

  function usePodcastArtwork(event: Event, fallback: string) {
    const image = event.currentTarget as HTMLImageElement;
    if (fallback && image.getAttribute('src') !== fallback) {
      image.src = fallback;
    } else {
      image.remove();
    }
  }

  function showLikedTracks() {
    abandonSearch();
    activeTab = 'music';
    showingLikedMusic = !showingLikedMusic;
    if (!showingLikedMusic) {
      void searchTracks(query);
      return;
    }
    tracks = [...likedMusic];
    total = tracks.length;
    selected = tracks[0] ?? null;
  }

  /**
   * Leaves the liked page, back where it was opened from.
   *
   * The only control that opens it is the "Liked" chip on the search page, so
   * that is where it returns to: the search that was running, or the search
   * page itself. Before this the chip moved the app to the music tab and the
   * page had no way out of its own.
   */
  function closeLikedMusic() {
    if (!showingLikedMusic) return;
    showingLikedMusic = false;
    activeTab = 'search';
    if (query.trim()) void searchTracks(query);
    else void loadLibrary();
  }

  /** A right swipe on the liked page, so the page can be thrown away by hand. */
  let likedSwipeTracking = false;
  let likedSwipeActive = $state(false);
  let likedSwipeX = $state(0);
  let likedSwipeStartX = 0;
  let likedSwipeStartY = 0;
  let likedSwipeSwallowClick = false;

  function startLikedSwipe(event: PointerEvent) {
    if (!showingLikedMusic || event.button !== 0) return;
    likedSwipeTracking = true;
    likedSwipeActive = false;
    likedSwipeSwallowClick = false;
    likedSwipeStartX = event.clientX;
    likedSwipeStartY = event.clientY;
    likedSwipeX = 0;
  }

  function moveLikedSwipe(event: PointerEvent) {
    if (!likedSwipeTracking) return;
    const travelX = event.clientX - likedSwipeStartX;
    const travelY = event.clientY - likedSwipeStartY;
    if (!likedSwipeActive) {
      // The list scrolls vertically, so only a clearly sideways pull takes the
      // gesture, and only to the right. Everything else stays the list's.
      if (Math.abs(travelX) < 12 || Math.abs(travelX) < Math.abs(travelY) * 1.5) return;
      if (travelX <= 0) {
        likedSwipeTracking = false;
        return;
      }
      likedSwipeActive = true;
    }
    likedSwipeX = Math.max(0, travelX);
  }

  function endLikedSwipe() {
    if (!likedSwipeTracking) return;
    likedSwipeTracking = false;
    if (!likedSwipeActive) return;
    likedSwipeActive = false;
    // A pull must not also press whatever was under the finger.
    likedSwipeSwallowClick = likedSwipeX > 8;
    const threshold = Math.min(window.innerWidth * LIKED_SWIPE_DISMISS_RATIO, 140);
    if (likedSwipeX > threshold) closeLikedMusic();
    likedSwipeX = 0;
  }

  /**
   * Swallows the click a swipe would otherwise become. A pressed track row
   * starts playing, so a pull that ends on one must not play anything.
   */
  function swallowLikedSwipeClick(event: MouseEvent) {
    if (!likedSwipeSwallowClick) return;
    likedSwipeSwallowClick = false;
    event.preventDefault();
    event.stopPropagation();
  }

  function randomIndexExcept(currentIndex: number) {
    if (playerQueue.length < 2) return -1;
    const played = new Set(randomHistory);
    const pool: number[] = [];
    for (let index = 0; index < playerQueue.length; index += 1) {
      if (index === currentIndex || played.has(index)) continue;
      pool.push(index);
    }
    // A finished cycle only starts again when the queue is set to repeat.
    if (pool.length === 0 && loopMode !== 'off') {
      for (let index = 0; index < playerQueue.length; index += 1) {
        if (index !== currentIndex) pool.push(index);
      }
    }
    if (pool.length === 0) return -1;
    return pool[Math.floor(Math.random() * pool.length)];
  }

  function resetRandomOrder() {
    randomHistory = playerIndex >= 0 ? [playerIndex] : [];
    randomHistoryIndex = randomHistory.length - 1;
    randomUpcoming = randomIndexExcept(playerIndex);
  }

  function savePlaySettings() {
    try {
      window.localStorage.setItem(playModeKey, JSON.stringify({ loop: loopMode, shuffle }));
    } catch {
      // A preference that cannot be stored is only a lost convenience.
    }
  }

  function cycleLoopMode() {
    loopMode = LOOP_MODES[(LOOP_MODES.indexOf(loopMode) + 1) % LOOP_MODES.length];
    // The unplayed pool depends on whether a finished cycle may start again.
    resetRandomOrder();
    savePlaySettings();
    syncSystemMedia(true);
  }

  function toggleShuffle() {
    shuffle = !shuffle;
    resetRandomOrder();
    savePlaySettings();
    syncSystemMedia(true);
  }

  /** Repeat on whichever player the drawer is showing. */
  function cycleShownRepeat() {
    if (playbackTarget === 'desktop') {
      if (status.streamOnly) return;
      void sendPlayback({ type: 'repeat', mode: nextRemoteRepeat(remoteState?.repeat ?? 'off') });
      return;
    }
    cycleLoopMode();
  }

  /** Shuffle on whichever player the drawer is showing. */
  function toggleShownShuffle() {
    if (playbackTarget === 'desktop') {
      if (status.streamOnly) return;
      void sendPlayback({ type: 'shuffle', enabled: remoteState?.shuffle !== true });
      return;
    }
    toggleShuffle();
  }

  /** The heart is this phone's own list for either player; see `syncSystemMedia`. */
  function toggleShownLike() {
    const track = shownTrack;
    if (track) toggleTrackLike(track);
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
        // Covers are cached here, the albums the host had nothing for included,
        // so the host's own count of how often its art changed is what tells
        // this phone to ask again instead of trusting an answer that has aged.
        invalidateCoverNegatives(status.coverRevision);
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
      // These are exactly the files this phone holds, which is the first of
      // the three storage states the badges describe.
      cachedFileIds = new Set(offline.tracks.map((track) => track.fileId));
      if (!selected || !tracks.some((track) => track.fileId === selected?.fileId)) selected = tracks[0] ?? null;
    } catch {
      // A damaged cache must never prevent pairing or normal online use.
    }
  }

  /** Re-reads the phone's audio cache after a download lands. */
  async function refreshCachedIds() {
    try {
      const offline = await invoke<LibraryPage>('cached_library');
      cachedFileIds = new Set(offline.tracks.map((track) => track.fileId));
    } catch {
      // Offline or a damaged cache: the badge falls back to host and network.
    }
  }

  /**
   * Records a file the host has just cached on this phone.
   *
   * Playing a track is how most music reaches the phone, because the host caches
   * it for offline playback. The badge only moved when a *download* finished, so
   * a song that arrived by being played went on claiming it was stored on the
   * computer until the next launch read the cache again.
   */
  function noteCachedOnPhone(fileId: string) {
    if (cachedFileIds.has(fileId)) return;
    cachedFileIds = new Set(cachedFileIds).add(fileId);
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
    status = { streamOnly: false, paired: false, connected: false, desktopName: '', endpointId: '', libraryRevision: 0, coverRevision: 0, error: '' };
    tracks = [];
    current = null;
    audio?.pause();
  }

  async function loadLibrary(append = false) {
    if (!status.paired || loading || loadingMore) return;
    const viewVersion = ++musicViewVersion;
    showingLikedMusic = false;
    searchingNetwork = false;
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

  /**
   * Give up on the search that is in flight. Its request still settles later,
   * but every `viewVersion` check in `searchTracks` is guarded against the stale
   * one, so nothing else would ever clear these flags — an abandoned search left
   * the list spinning forever and refused the library reload that followed it.
   */
  function abandonSearch() {
    musicViewVersion += 1;
    loading = false;
    loadingMore = false;
    searchingNetwork = false;
  }

  async function searchTracks(nextQuery = query) {
    query = nextQuery;
    showingLikedMusic = false;
    // Clearing the box abandons the search in flight and goes back to the library.
    if (!query.trim()) {
      abandonSearch();
      return loadLibrary();
    }
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
        // A network answer must not downgrade a track already on the host.
        if (!merged.get(track.fileId)?.local || track.local) merged.set(track.fileId, track);
      }
      tracks = [...merged.values()].sort((left, right) => Number(right.local) - Number(left.local));
      total = tracks.length;
      selected = tracks.find((track) => track.fileId === selected?.fileId) ?? tracks[0] ?? null;
    };
    // Both requests go out together: the host answers for its own folder, the
    // network for everything else, and neither waits for the other. A query
    // typed while they are in flight supersedes them rather than being ignored.
    const localSearch = invoke<LibraryPage>('remote_library', {
      query: searchQuery,
      offset: 0,
      limit: MAX_ALBUM_TRACKS
    })
      .then((page) => mergeResults(page.tracks))
      .catch((nextError) => { if (viewVersion === musicViewVersion) error = String(nextError); })
      .finally(() => { if (viewVersion === musicViewVersion) loading = false; });
    const networkSearch = searchingNetwork
      ? invoke<RemoteTrack[]>('remote_search', { query: searchQuery })
        .then(mergeResults)
        .catch((nextError) => {
          if (viewVersion !== musicViewVersion) return;
          // The host's own files are still worth showing when the network is out.
          if (tracks.length > 0) notice = msg("Showing results from Napstr only: {p0}", { p0: String(nextError) });
          else error = String(nextError);
        })
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
    if (playbackTarget === 'desktop') return playOnDesktop(track);
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
      // The file is on this phone now, so the badge has to stop saying otherwise.
      noteCachedOnPhone(cached.track.fileId);
      // The new source has no length until it reports one: keeping the old one
      // would show the previous track's length, and seek against it.
      duration = 0;
      durationEstimated = false;
      await tick();
      audio.src = cached.url;
      audio.volume = volume;
      await audio.play();
      playing = true;
      rememberPlayedAlbum(cached.track);
      const nextIndex = shuffle
        ? randomUpcoming
        : playerQueue.length > 1
          ? (playerIndex + 1 < playerQueue.length ? playerIndex + 1 : loopMode === 'off' ? -1 : 0)
          : -1;
      const next = nextIndex >= 0 ? playerQueue[nextIndex] : undefined;
      if (next?.local) {
        void invoke('prefetch_remote_audio', {
          afterFileId: cached.track.fileId,
          track: next,
          libraryVisible
        });
      }
      // The same track's artwork is asked about and fetched now, so the player
      // has a cover the moment it starts instead of after a round trip.
      if (next) preloadArtwork(next);
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
    if (status.streamOnly) {
      error = 'This pairing is read only. It cannot ask Napstr to download songs.';
      return;
    }
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
        void refreshCachedIds();
      }
    } catch { /* the next foreground poll retries */ }
  }

  function togglePlayer() {
    // The bar's button belongs to whichever player the bar is showing.
    if (playbackTarget === 'desktop') {
      if (remoteBusy || status.streamOnly) return;
      // Nothing loaded over there: the computer decides what "play" means, and
      // the host asks its own window to pick up where it left off.
      void sendPlayback(remoteState?.active ? { type: 'toggle' } : { type: 'play' });
      return;
    }
    if (!current && !currentPodcast) return;
    if (audio.paused) audio.play().catch((nextError) => (error = String(nextError)));
    else audio.pause();
  }

  /**
   * Ask the system's media controls to catch up with the player this phone is
   * listening to.
   *
   * `force` skips the coalescing for the changes that would look wrong until
   * it came round on its own: a new track, a pause, a duration that just
   * arrived.
   */
  function syncSystemMedia(force = false) {
    if (!force) return mediaUpdates.request();
    mediaUpdates.cancel();
    publishSystemMedia();
  }

  /**
   * Tell Android what its media controls should be showing.
   *
   * The lock screen, the notification, Android Auto and every Bluetooth button
   * speak this one session, so it has to describe the player the user is
   * actually listening to: the computer when it is the source, this phone
   * otherwise. Position and duration go over in seconds, and only when they are
   * a length the native session can hold.
   */
  function publishSystemMedia() {
    if (!androidMediaBridge() && !webMediaSession()) return;

    if (playbackTarget === 'desktop') {
      const state = remoteState;
      if (!state?.active) {
        // Nothing playing over there means nothing to show: this phone stopped
        // its own playback when the computer took the source over.
        if (systemMediaEmpty) return;
        systemMediaEmpty = true;
        stopSystemMedia();
        return;
      }
      systemMediaEmpty = false;
      const seconds = validDuration((state.durationMs ?? 0) / 1000);
      publishSystemMetadata({
        title: state.title || 'Unknown track',
        artist: state.artist || status.desktopName || 'The computer',
        artwork: lockScreenArtwork(),
        playing: state.playing,
        position: safePosition(remotePositionMs() / 1000, seconds || undefined),
        duration: seconds,
        canPrevious: state.queueLen > 1,
        canNext: state.queueLen > 1,
        canSeek: seconds > 0,
        labels: mediaLabels(),
        // The heart is still this phone's own list, exactly as it is for local
        // playback: liking something never reaches the computer.
        liked: shownTrack ? isTrackLiked(shownTrack) : false,
        looping: state.repeat !== 'off',
        volume: remoteVolumePercent(),
        // Not this phone's audio: the service must not hold the screen for it,
        // and its volume keys belong to the computer's player instead.
        remote: true
      });
      return;
    }

    const media = activeMedia === 'podcast' ? currentPodcast : current;
    if (!media) {
      // This phone has nothing to describe either, so the session must stop
      // describing the computer's track rather than leaving it on the lock
      // screen after the source has moved back here.
      if (systemMediaEmpty) return;
      systemMediaEmpty = true;
      stopSystemMedia();
      return;
    }
    systemMediaEmpty = false;
    const seconds = validDuration(verifiedDuration);
    publishSystemMetadata({
      title: activeMedia === 'podcast' ? currentPodcast?.title ?? '' : current ? title(current) : '',
      artist: activeMedia === 'podcast' ? currentPodcast?.feedTitle ?? '' : current ? artist(current) : '',
      artwork: activeMedia === 'podcast'
        ? currentPodcast?.image ?? ''
        : lockScreenArtwork(),
      playing,
      position: safePosition(currentTime, seconds || undefined),
      duration: seconds,
      canPrevious: activeMedia === 'music' && playerQueue.length > 1 && (!shuffle || randomHistoryIndex > 0),
      canNext: activeMedia === 'music' && playerQueue.length > 1,
      canSeek: seconds > 0 && !caching,
      labels: mediaLabels(),
      liked: activeMedia === 'music' && !!current && isTrackLiked(current),
      looping: activeMedia === 'music' && loopMode !== 'off',
      volume: Math.round(volume * 100),
      remote: false
    });
  }

  /** No Android bridge means a desktop window, which is driven through the web session. */
  function webMediaSession(): MediaSession | undefined {
    return 'mediaSession' in navigator ? navigator.mediaSession : undefined;
  }

  /**
   * Publish what is playing to whichever system surface this build has. On a
   * phone that is the app's own media service, which draws the notification and
   * the lock screen; in a desktop window it is the web session, which the OS
   * reads for its own player and its media keys.
   */
  function publishSystemMetadata(metadata: SystemMediaState) {
    const bridge = androidMediaBridge();
    if (bridge) {
      bridge.update(JSON.stringify(metadata));
      return;
    }
    const session = webMediaSession();
    if (!session) return;
    try {
      // A progress bar needs a length. Without one, say nothing rather than
      // publish a made-up duration at it.
      if (!metadata.duration) {
        if (lastSessionPosition) stopSystemMedia();
        return;
      }
      const position = {
        duration: metadata.duration,
        playbackRate: 1,
        position: Math.min(metadata.position, metadata.duration)
      };
      const positionKey = JSON.stringify(position);
      if (positionKey !== lastSessionPosition) {
        session.setPositionState(position);
        lastSessionPosition = positionKey;
      }
      const metadataKey = JSON.stringify([metadata.title, metadata.artist, metadata.artwork ?? '']);
      if (metadataKey !== lastSessionMetadata && 'MediaMetadata' in window) {
        session.metadata = new MediaMetadata({ title: metadata.title, artist: metadata.artist });
        lastSessionMetadata = metadataKey;
      }
      const state = metadata.playing ? 'playing' : 'paused';
      if (state !== lastSessionState) {
        session.playbackState = state;
        lastSessionState = state;
      }
    } catch {
      // Some webviews expose only part of Media Session.
    }
  }

  /** Take the track down from the system's controls, on either surface. */
  function stopSystemMedia() {
    const bridge = androidMediaBridge();
    if (bridge) {
      bridge.clear();
      return;
    }
    const session = webMediaSession();
    lastSessionPosition = '';
    lastSessionMetadata = '';
    lastSessionState = '';
    if (!session) return;
    try {
      session.metadata = null;
      session.playbackState = 'none';
      session.setPositionState();
    } catch {
      // Some webviews expose only part of Media Session.
    }
  }

  /**
   * A desktop window has no Android service to receive its player's buttons, so
   * the media keys, the OS player and the keyboard arrive here instead.
   */
  function setupMediaSession() {
    if (androidMediaBridge() || !('mediaSession' in navigator)) return () => {};
    const handlers: Array<[MediaSessionAction, MediaSessionActionHandler]> = [
      ['play', () => { if (audio?.paused) togglePlayer(); }],
      ['pause', () => audio?.pause()],
      ['previoustrack', () => { void moveTrackBy(-1); }],
      ['nexttrack', () => { void moveTrackBy(1); }],
      ['seekto', (event) => { if (event.seekTime !== undefined) void seekShown(event.seekTime); }],
      ['seekbackward', (event) => void nudgeShown(-(event.seekOffset ?? 15))],
      ['seekforward', (event) => void nudgeShown(event.seekOffset ?? 15)]
    ];
    const registered: MediaSessionAction[] = [];
    for (const [action, handler] of handlers) {
      try {
        navigator.mediaSession.setActionHandler(action, handler);
        registered.push(action);
      } catch {
        // An action this webview does not know must not cost the others.
      }
    }
    return () => {
      for (const action of registered) navigator.mediaSession.setActionHandler(action, null);
    };
  }

  /**
   * A desk has a keyboard, and a phone can have one attached. Space and the
   * arrow keys do what the transport buttons do, as long as nothing else is
   * listening - no field is being typed into and no drawer is open on top.
   */
  function handleKeyboard(event: KeyboardEvent) {
    if (event.defaultPrevented || event.isComposing || showSettings) return;
    if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === 'f') {
      const search = document.querySelector<HTMLInputElement>('.search-area input');
      if (search) {
        event.preventDefault();
        search.focus();
        search.select();
      }
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.repeat) return;
    if (event.target instanceof HTMLElement
      && event.target.closest('input, textarea, select, button, a, summary, [contenteditable="true"]')) return;
    if (!current && !currentPodcast) return;
    if (event.code === 'Space') {
      event.preventDefault();
      togglePlayer();
    } else if (event.key === 'ArrowLeft') {
      event.preventDefault();
      void nudgeShown(-15);
    } else if (event.key === 'ArrowRight') {
      event.preventDefault();
      void nudgeShown(15);
    }
  }

  /**
   * The notification's own buttons are drawn by Android, so their words have to
   * travel with the state: this phone is what knows which language the reader
   * chose, and which of the two like and repeat labels applies right now.
   */
  function mediaLabels() {
    return {
      previous: $t('Previous track'),
      rewind: $t('Back 15 seconds'),
      play: $t('Play'),
      pause: $t('Pause'),
      forward: $t('Forward 15 seconds'),
      next: $t('Next track'),
      channel: $t('Media playback'),
      like: $t('Add to Liked Songs'),
      unlike: $t('Remove from Liked Songs'),
      repeat: $t('Repeat'),
      repeatOff: $t('Turn repeat off')
    };
  }

  function handleSystemMediaAction(event: Event) {
    const action = (event as CustomEvent<string>).detail;

    // The computer is the player, so every button belongs to it. This phone's
    // audio has been stopped since it handed the source over.
    if (playbackTarget === 'desktop') {
      if (action === 'play') void sendPlayback({ type: 'play' });
      else if (action === 'pause') void sendPlayback({ type: 'pause' });
      else if (action === 'previous') void moveTrackBy(-1);
      else if (action === 'next') void moveTrackBy(1);
      else if (action === 'like') toggleShownLike();
      else if (action === 'repeat') cycleShownRepeat();
      else if (action === 'rewind') void nudgeShown(-15);
      else if (action === 'forward') void nudgeShown(15);
      else if (action === 'volumeUp') adjustRemoteVolume(1);
      else if (action === 'volumeDown') adjustRemoteVolume(-1);
      else if (action.startsWith('seek:')) {
        const milliseconds = Number(action.slice(5));
        if (Number.isFinite(milliseconds)) void seekShown(milliseconds / 1000);
      }
      return;
    }

    if (!audio) return;
    if (action === 'play') {
      if (audio.paused) audio.play().catch((nextError) => (error = String(nextError)));
    } else if (action === 'pause') {
      if (!audio.paused) audio.pause();
    } else if (action === 'previous') {
      void moveTrack(-1);
    } else if (action === 'next') {
      void moveTrack(1);
    } else if (action === 'like') {
      if (current && activeMedia === 'music') toggleTrackLike(current);
    } else if (action === 'repeat') {
      // The notification toggles repeat; the drawer owns the three-way choice.
      if (activeMedia === 'music') {
        loopMode = loopMode === 'off' ? 'all' : 'off';
        resetRandomOrder();
        savePlaySettings();
        syncSystemMedia(true);
      }
    } else if (action === 'rewind') {
      void nudgeShown(-15);
    } else if (action === 'forward') {
      void nudgeShown(15);
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

  // Track the drawer's cover alongside whichever player it is showing. The
  // batched cache means this is free when the library list already resolved the
  // same album.
  $effect(() => {
    const track = shownTrack;
    // Key on the fields the lookup uses: a fresh object for the same track (the
    // computer reports a new one every few seconds) must not drop the art.
    const key = track ? `${track.fileId}\n${track.artist}\n${track.album}` : '';
    if (key === nowCoverKey) return;
    nowCoverKey = key;
    nowArtFailed = false;
    nowCover = null;
    sheetArtLoaded = '';
    if (!track) return;
    let alive = true;
    void coverFor(track).then((cover) => {
      if (!alive) return;
      nowCover = cover;
      syncSystemMedia(true);
    });
    return () => { alive = false; };
  });

  $effect(() => {
    if (!current) {
      showNowPlaying = false;
      showQueue = false;
    }
  });

  // The lock screen starts on the thumbnail, which is already here, and moves to
  // the full cover once that has landed. For a track that was reached by playing
  // the one before it, the preload means this is a cache hit and the upgrade
  // follows within a frame or two of the notification appearing.
  $effect(() => {
    const cover = nowCover;
    lockScreenCover = cover?.thumb ?? '';
    const full = cover?.art ?? '';
    if (!full || full === cover?.thumb) return;
    let alive = true;
    void loadFullCover(cover).then((landed) => {
      if (!alive || !landed) return;
      lockScreenCover = landed;
      // Not forced: nothing here is wrong-looking until the coalescer comes round.
      syncSystemMedia();
    });
    return () => { alive = false; };
  });

  // While a drag is in flight the browser must not claim the gesture as a
  // scroll: these listeners are deliberately non-passive so they can stop that.
  $effect(() => {
    const targets: HTMLElement[] = [];
    if (sheetElement) targets.push(sheetElement);
    if (barElement) targets.push(barElement);
    if (targets.length === 0) return;
    const hold: EventListener = (event) => {
      if (sheetDragging || barDragging) event.preventDefault();
    };
    for (const target of targets) target.addEventListener('touchmove', hold, { passive: false });
    return () => {
      for (const target of targets) target.removeEventListener('touchmove', hold);
    };
  });

  /** Albums this phone has loaded, which is what both shelves are built from. */
  let libraryAlbums = $derived(albumsFromTracks(tracks));
  /** Search results grouped the way the results view presents them. */
  let resultArtists = $derived(artistsFromTracks(tracks));
  let searching = $derived(Boolean(query.trim()) && !showingLikedMusic);

  let lastPlayed = $derived(
    playedAlbums
      .map((played) => libraryAlbums.find((album) => album.key === played.key))
      .filter((album): album is AlbumShelf => Boolean(album))
      .slice(0, 12)
  );

  // Reshuffle only when the album set itself changes, so the shelf does not
  // jump around while the user is looking at it.
  $effect(() => {
    const albums = libraryAlbums;
    const seed = albums.map((album) => album.key).sort().join('|');
    if (seed === discoverSeed) return;
    discoverSeed = seed;
    discoverAlbums = shuffled(albums).slice(0, 12);
  });

  $effect(() => {
    document.body.style.overflow = showNowPlaying ? 'hidden' : '';
    return () => { document.body.style.overflow = ''; };
  });

  // A notice is a toast, not a banner: it says what just happened and then takes
  // itself away, so nothing accumulates at the top of a long session. A second
  // message restarts the clock rather than queueing behind the first.
  $effect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => { notice = ''; }, NOTICE_VISIBLE_MS);
    return () => window.clearTimeout(timer);
  });

  /**
   * Everything a back press could close, in the order `handleSystemBack` walks
   * it: the overlays over the drawer, then the drawer, then the pages below it.
   * The liked page and the tabs count too, because they are pages the user can
   * be left standing on.
   */
  let backHasDestination = $derived(
    showReport ||
      (showSourceOptions && !showActions) ||
      showSettings ||
      showActions ||
      showAlbumView ||
      showQueue ||
      (showNowPlaying && !sheetClosing) ||
      showingLikedMusic ||
      activeTab !== 'music'
  );

  /**
   * Counted by every press the page handles.
   *
   * Android clears the flag when it gives the page a press, because one press is
   * one answer. A press that closes the liked page lands on the search page,
   * which is also somewhere back can go, so the answer does not change and
   * nothing derived from it will fire on its own: without this counter the flag
   * would stay cleared and the app would be left by the press after that.
   */
  let backPresses = $state(0);

  // The page, and every view above it, own the hardware back button.
  $effect(() => {
    void backPresses;
    pushBackAvailability(backHasDestination);
  });

  function nowPlayingAvailable() {
    if (playbackTarget === 'desktop') return remoteAvailable();
    return activeMedia === 'music' && playerQueueLibraryVisible && !!current;
  }

  function openNowPlaying() {
    if (!nowPlayingAvailable()) return;
    // Tapping the bar mid-close should bring the drawer back, not be ignored.
    window.clearTimeout(sheetCloseTimer);
    if (showNowPlaying && !sheetClosing) return;
    sheetClosing = false;
    sheetDragY = 0;
    if (showNowPlaying) return;
    // Remember where the bar sits: the drawer's travel and the navigation bar's
    // slide are both measured against it.
    barRestTop = barElement?.getBoundingClientRect().top ?? window.innerHeight;
    sheetEntering = true;
    showNowPlaying = true;
    // Remove the start position on the next frame so the transition runs.
    window.requestAnimationFrame(() => { sheetEntering = false; });
  }

  function closeNowPlaying() {
    if (!showNowPlaying || sheetClosing) return;
    sheetClosing = true;
    sheetDragging = false;
    sheetCloseTimer = window.setTimeout(() => {
      showNowPlaying = false;
      sheetClosing = false;
      sheetDragY = 0;
    }, SHEET_ANIMATION_MS);
  }

  function sheetCoverUrl() {
    if (!nowCover || nowArtFailed) return '';
    return nowCover.art || nowCover.thumb;
  }

  /**
   * The small rendition, drawn from the moment the drawer opens.
   *
   * The list that was tapped has already fetched this one, so it is in the
   * image cache; the full front cover is a fresh download and would otherwise
   * leave the sheet black for as long as it takes to arrive.
   */
  function sheetThumbUrl() {
    return nowCover?.thumb ?? '';
  }

  /**
   * The artwork the system is given for what is playing.
   *
   * The thumbnail goes first, because it is the rendition already on this phone,
   * and the full cover replaces it as soon as that one has landed: the media
   * service takes a new URL for the same art and re-posts without alerting again.
   * A full cover that will not load leaves the thumbnail in place.
   */
  function lockScreenArtwork(): string {
    if (!nowCover) return '';
    return lockScreenCover || nowCover.thumb || nowCover.art;
  }

  async function playFromQueue(index: number) {
    const track = playerQueue[index];
    if (!track) return;
    playerIndex = index;
    selected = track;
    resetRandomOrder();
    await playTrack(track);
  }

  /** Play a row of the playlist on whichever player it belongs to. */
  async function playQueueRow(index: number) {
    if (playbackTarget === 'desktop') {
      await playFromRemoteQueue(index);
      return;
    }
    await playFromQueue(index);
  }

  function startSheetDrag(event: PointerEvent) {
    // A pinned column is not a drawer: there is nothing to pull down or dismiss.
    if (pinned || sheetClosing) return;
    // Sliders own their own gestures.
    if ((event.target as HTMLElement | null)?.closest('input')) return;
    const target = (event.target as Node | null) ?? null;
    sheetDragPending = true;
    sheetDragging = false;
    sheetDragStart = event.clientY;
    sheetScrollTop = sheetScroller?.scrollTop ?? 0;
    // A gesture starting inside the queue belongs to the queue until it is back
    // at its top; everything above it can be pulled away at once.
    sheetDragAllowed = !(target && sheetScroller?.contains(target)) || sheetScrollTop <= 0;
  }

  function moveSheetDrag(event: PointerEvent) {
    const travel = event.clientY - sheetDragStart;
    if (sheetDragPending) {
      if (!sheetDragAllowed && travel > 6) {
        sheetDragPending = false;
        return;
      }
      // Wait for a real downward pull before stealing the gesture.
      if (travel < 6) {
        if (travel > -6) return;
        sheetDragPending = false;
        return;
      }
      sheetDragPending = false;
      sheetDragging = true;
    }
    if (!sheetDragging) return;
    sheetDragY = Math.max(0, travel);
  }

  function endSheetDrag() {
    sheetDragPending = false;
    if (!sheetDragging) return;
    sheetDragging = false;
    const height = sheetElement?.offsetHeight ?? window.innerHeight;
    // Pulling the drawer a fifth of the way down commits to closing.
    if (sheetDragY > height * SHEET_DISMISS_RATIO) closeNowPlaying();
    else sheetDragY = 0;
  }

  function startBarDrag(event: PointerEvent) {
    if (!nowPlayingAvailable() || showNowPlaying) return;
    barDragging = true;
    barDragTravelled = 0;
    barDragSpeed = 0;
    barDragStart = event.clientY;
    barDragLastY = event.clientY;
    barDragLastAt = performance.now();
    barRestTop = barElement?.getBoundingClientRect().top ?? window.innerHeight;
    // Keep every move on the bar even once the drawer covers it.
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }

  function moveBarDrag(event: PointerEvent) {
    if (!barDragging) return;
    const now = performance.now();
    const elapsed = now - barDragLastAt;
    if (elapsed > 0) barDragSpeed = (barDragLastY - event.clientY) / elapsed;
    barDragLastY = event.clientY;
    barDragLastAt = now;
    barDragTravelled = barDragStart - event.clientY;
    if (barDragTravelled <= BAR_DRAG_SLOP) return;
    if (!showNowPlaying) {
      // Mount the drawer with its top edge level with the bar, so it rises out
      // from underneath it rather than appearing at the foot of the page.
      sheetClosing = false;
      sheetEntering = false;
      sheetDragging = true;
      showNowPlaying = true;
    }
    sheetDragY = Math.max(0, barRestTop - barDragTravelled);
  }

  function endBarDrag() {
    if (!barDragging) return;
    barDragging = false;
    const travelled = barDragTravelled;
    barDragTravelled = 0;
    // A deliberate pull should not also fire the button's click handler.
    barSwallowClick = travelled > 8;
    if (!showNowPlaying) return;
    sheetDragging = false;
    // The pull that fades the bar out is the pull that opens the drawer, so
    // the release decision matches what the finger just saw.
    if (travelled > barOpenTravel || barDragSpeed > BAR_FLING_SPEED) sheetDragY = 0;
    else closeNowPlaying();
  }

  function handleBarClick() {
    if (barSwallowClick) {
      barSwallowClick = false;
      return;
    }
    openNowPlaying();
  }

  /** Seek by a relative amount on whichever player the drawer is showing. */
  async function nudgeShown(seconds: number) {
    // Local playback is read from the element itself: the OS asks for a seek
    // between two `timeupdate` events, and our copied position would still be
    // the old one, so "back 15" could land somewhere the user did not ask for.
    const position = playbackTarget === 'desktop' || !audio ? shownPosition : audio.currentTime;
    const limit = verifiedDuration > 0 ? verifiedDuration : Number.POSITIVE_INFINITY;
    await seekShown(Math.min(Math.max(0, position + seconds), limit));
  }

  /** Seek on whichever player the drawer is showing. */
  async function seekShown(seconds: number) {
    if (playbackTarget === 'desktop') {
      if (status.streamOnly) return;
      const limit = (remoteState?.durationMs ?? 0) / 1000;
      const target = limit > 0 ? Math.min(Math.max(0, seconds), limit) : Math.max(0, seconds);
      await sendPlayback({ type: 'seek', positionMs: Math.round(target * 1000) });
      return;
    }
    seek(seconds);
  }

  /** Albums among the loaded tracks, grouped by the key covers are addressed by. */
  function albumsFromTracks(source: RemoteTrack[]): AlbumShelf[] {
    const groups = new Map<string, AlbumShelf>();
    for (const track of source) {
      const key = coverKey(track.artist ?? '', track.album ?? '');
      if (!key) continue;
      const existing = groups.get(key);
      if (existing) existing.tracks.push(track);
      else groups.set(key, {
        key,
        artist: track.artist,
        album: track.album,
        representative: track,
        tracks: [track]
      });
    }
    return [...groups.values()];
  }

  /**
   * Artists among a set of tracks. There is no artist record on the wire, so a
   * search groups the results it already has rather than asking the host for
   * something it does not model.
   */
  function artistsFromTracks(source: RemoteTrack[]): ArtistShelf[] {
    const groups = new Map<string, ArtistShelf>();
    for (const track of source) {
      const name = (track.artist ?? '').trim();
      if (!name) continue;
      const key = name.toLocaleLowerCase();
      const existing = groups.get(key);
      if (existing) existing.count += 1;
      else groups.set(key, { name, representative: track, count: 1 });
    }
    return [...groups.values()].sort((left, right) => right.count - left.count);
  }

  /** Other albums by the same artist, for the "More by" carousel. */
  async function albumsByArtist(album: AlbumShelf): Promise<AlbumShelf[]> {
    const name = album.artist.trim();
    if (!name) return [];
    const wanted = name.toLocaleLowerCase();
    const known = libraryAlbums.filter(
      (entry) => entry.key !== album.key && entry.artist.trim().toLocaleLowerCase() === wanted
    );
    try {
      const page = await invoke<LibraryPage>('remote_library', {
        query: name, offset: 0, limit: MAX_ALBUM_TRACKS
      });
      const sameArtist = page.tracks.filter(
        (track) => (track.artist ?? '').trim().toLocaleLowerCase() === wanted
      );
      const found = albumsFromTracks(sameArtist).filter((entry) => entry.key !== album.key);
      for (const entry of known) {
        if (!found.some((other) => other.key === entry.key)) found.push(entry);
      }
      return found.slice(0, 12);
    } catch {
      // Offline: whatever the loaded page already holds.
      return known.slice(0, 12);
    }
  }

  /**
   * Opening an album previews it: the tracks, the year the cover NIP carries,
   * and what else the artist has here. Playback starts from the view.
   */
  async function openAlbum(album: AlbumShelf, prepared?: RemoteTrack[]) {
    // The album page is a full screen of its own. Leaving the drawer open would
    // stack two full-height views, and the album is what the tap asked for.
    if (showNowPlaying) closeNowPlaying();
    closeActions();
    showAlbumView = true;
    albumArtLoaded = '';
    albumView = {
      key: album.key,
      artist: album.artist,
      album: album.album,
      year: '',
      art: '',
      thumb: '',
      tracks: prepared ?? album.tracks,
      more: []
    };
    const [tracks, cover, more] = await Promise.all([
      prepared ? Promise.resolve(prepared) : albumPlaylist(album),
      coverFor(album.representative).catch(() => null),
      albumsByArtist(album)
    ]);
    // A later open wins, so a slow response cannot overwrite a newer album.
    if (albumView?.key !== album.key) return;
    albumView = {
      key: album.key,
      artist: album.artist,
      album: album.album,
      year: cover?.year ?? '',
      art: cover ? cover.art || cover.thumb : '',
      thumb: cover?.thumb ?? '',
      tracks,
      more
    };
  }

  function closeAlbumView() {
    showAlbumView = false;
    albumView = null;
    albumArtLoaded = '';
  }

  /** Opens the same album preview from a track, wherever the menu was opened. */
  async function goToAlbum(track: RemoteTrack) {
    closeActions();
    const owner = albumsFromTracks([track])[0];
    if (!owner) return;
    const full = await albumPlaylist(owner);
    await openAlbum({ ...owner, tracks: full.length > 0 ? full : owner.tracks }, full);
  }

  function goToArtist(name: string) {
    closeActions();
    closeAlbumView();
    if (showNowPlaying) closeNowPlaying();
    activeTab = 'music';
    void searchTracks(name.trim());
  }

  async function playAlbumNow() {
    const view = albumView;
    if (!view) return;
    if (playbackTarget === 'desktop') {
      const first = view.tracks[0];
      if (first) await playOnDesktop(first);
      return;
    }
    const playable = view.tracks.filter((track) => track.local);
    if (playable.length === 0) {
      await activateTrack(view.tracks[0]);
      return;
    }
    playerQueue = playable;
    playerQueueLibraryVisible = true;
    playerIndex = 0;
    selected = playable[0];
    resetRandomOrder();
    await playTrack(playable[0]);
  }

  async function playAlbumTrack(index: number) {
    const view = albumView;
    const track = view?.tracks[index];
    if (!view || !track) return;
    if (playbackTarget === 'desktop') {
      await playOnDesktop(track);
      return;
    }
    if (!track.local) {
      await requestDownload(track);
      return;
    }
    const playable = view.tracks.filter((item) => item.local);
    playerQueue = playable;
    playerQueueLibraryVisible = true;
    playerIndex = playable.findIndex((item) => item.fileId === track.fileId);
    selected = track;
    resetRandomOrder();
    await playTrack(track);
  }

  /** Whether this phone has anything to say to the computer's player. */
  function remoteAvailable() {
    return status.paired && status.connected;
  }

  /** Whether the computer could actually take playback over right now. */
  function desktopTargetAvailable() {
    return remoteAvailable() && !status.streamOnly;
  }

  /** The computer's current track, as something the bar can draw. */
  function desktopTrackFromState(): RemoteTrack | null {
    const playing = remoteState;
    if (!playing?.active || !playing.fileId) return null;
    const known =
      tracks.find((item) => item.fileId === playing.fileId) ??
      likedMusic.find((item) => item.fileId === playing.fileId);
    // The library copy brings the artwork lookup and the badges; the computer's
    // own words win where it has them, so the bar, the drawer and the lock
    // screen cannot describe the same track differently.
    if (known) {
      return {
        ...known,
        title: playing.title || known.title,
        artist: playing.artist || known.artist,
        album: playing.album || known.album
      };
    }
    return {
      fileId: playing.fileId,
      filename: '',
      title: playing.title,
      artist: playing.artist,
      album: playing.album,
      format: '',
      mime: '',
      size: 0,
      tags: '',
      local: false,
      sources: []
    };
  }

  /** Where a tap will play, in the words the sheets use. */
  function playbackTargetLabel() {
    return playbackTarget === 'desktop' ? status.desktopName || 'The computer' : 'This phone';
  }

  /**
   * The list this phone is showing around a track, which becomes the computer's
   * queue. "Play what I am looking at" has to mean the same thing on both
   * devices, so the view decides it rather than the player's own history.
   */
  function visibleQueue(track: RemoteTrack): RemoteTrack[] {
    const view = albumView;
    if (view?.tracks.some((item) => item.fileId === track.fileId)) return view.tracks;
    if (showingLikedMusic && likedMusic.some((item) => item.fileId === track.fileId)) return likedMusic;
    return tracks.some((item) => item.fileId === track.fileId) ? tracks : [track];
  }

  function openSourcePicker() {
    showSleepOptions = false;
    showSourceOptions = true;
  }

  /** Opened from Settings, where there is no track menu to nest the rows in. */
  function openSourcePickerAlone() {
    showSettings = false;
    showActions = false;
    actionTrack = null;
    openSourcePicker();
  }

  function choosePlaybackTarget(target: PlaybackTarget) {
    playbackTarget = target;
    showSourceOptions = false;
    remoteVolume = -1;
    // One player at a time: handing over stops this phone. The drawer stays
    // open, because from here on it is showing the computer's player instead.
    if (target === 'desktop') {
      audio?.pause();
      playing = false;
      void refreshRemote();
    }
    syncSystemMedia(true);
  }

  /**
   * Play a track, and the list around it, on the computer.
   *
   * The list is what makes "next" mean over there what it means here, so it is
   * sent whole and capped only by what one request can carry. It is also the
   * only copy of the computer's queue this phone will ever have, which is why it
   * is kept.
   */
  async function playOnDesktop(track: RemoteTrack) {
    if (!desktopTargetAvailable()) return;
    audio?.pause();
    playing = false;
    selected = track;
    const queue = visibleQueue(track).slice(0, MAX_DESKTOP_QUEUE);
    remoteQueue = queue.length > 0 ? queue : [track];
    await sendPlayback({ type: 'playTrack', fileId: track.fileId, queue: remoteQueue.map((item) => item.fileId) });
    // The answer describes the computer as it was when the command arrived, so
    // it is asked again once it has had time to open the track.
    window.setTimeout(() => void refreshRemote(), 900);
  }

  /** Play a row of the copy of the computer's queue, keeping the rest of it. */
  async function playFromRemoteQueue(index: number) {
    const track = remoteQueue[index];
    if (!track || !desktopTargetAvailable()) return;
    selected = track;
    await sendPlayback({
      type: 'playTrack',
      fileId: track.fileId,
      queue: remoteQueue.map((item) => item.fileId)
    });
    window.setTimeout(() => void refreshRemote(), 900);
  }

  async function refreshRemote() {
    if (!remoteAvailable() || remoteFetching) return;
    remoteFetching = true;
    try {
      applyRemoteState(await invoke<RemotePlaybackState>('remote_playback_state'));
      remoteError = '';
    } catch (nextError) {
      remoteError = String(nextError);
    } finally {
      remoteFetching = false;
    }
  }

  /** Every transport button lands here, so the view never has to guess. */
  async function sendPlayback(command: PlaybackCommand) {
    if (remoteBusy) return;
    remoteBusy = true;
    try {
      applyRemoteState(await invoke<RemotePlaybackState>('remote_playback', { command }));
      remoteError = '';
    } catch (nextError) {
      remoteError = String(nextError);
    } finally {
      remoteBusy = false;
    }
  }

  /**
   * Volume follows a drag, so it cannot be gated on one command being in
   * flight; the last answer to arrive is the one shown.
   */
  async function sendVolume(percent: number) {
    if (!remoteAvailable()) return;
    try {
      applyRemoteState(
        await invoke<RemotePlaybackState>('remote_playback', {
          command: { type: 'volume', percent: Math.round(percent) } satisfies PlaybackCommand
        })
      );
      remoteError = '';
    } catch (nextError) {
      remoteError = String(nextError);
    }
  }

  /** Remember a state the computer reported, and when it reached this phone. */
  function applyRemoteState(state: RemotePlaybackState) {
    if (state.fileId !== remoteState?.fileId) remoteEndedFileId = '';
    remoteState = state;
    remoteStateAt = Date.now();
    // A sleep timer waiting for the end of a track over there. This phone has no
    // audio to listen to, so a different file being reported is the end of it.
    if (sleepValue === 'track') {
      if (!sleepRemoteFileId && state.fileId) sleepRemoteFileId = state.fileId;
      else if (sleepRemoteFileId && state.fileId !== sleepRemoteFileId) {
        sleepRemoteFileId = '';
        finishSleepTimer();
        return;
      }
    }
    syncSystemMedia();
  }

  /** The sleep timer is done: nothing anywhere should still be making a sound. */
  function finishSleepTimer() {
    sleepValue = '';
    sleepEndsAt = 0;
    sleepClock = '';
    pauseEverything();
    notice = 'Sleep timer finished';
  }

  /**
   * Silence both players. A sleep timer means no sound from anywhere, so this
   * stops the phone and asks the computer to stop too when it is the one playing.
   */
  function pauseEverything() {
    audio?.pause();
    playing = false;
    if (remoteState?.playing && desktopTargetAvailable()) void sendPlayback({ type: 'pause' });
  }

  /** The computer's volume as a percentage, whether or not the last change landed. */
  function remoteVolumePercent() {
    return remoteVolume >= 0 ? remoteVolume : Math.round((remoteState?.volume ?? 1) * 100);
  }

  /**
   * Set the computer's volume, from the drawer's slider or from a volume key.
   * A drag and a held key both arrive as a burst, so the request waits for the
   * burst to stop rather than becoming one request per event.
   */
  function setRemoteVolume(percent: number) {
    if (!desktopTargetAvailable()) return;
    remoteVolume = Math.min(100, Math.max(0, Math.round(percent)));
    syncSystemMedia(true);
    window.clearTimeout(remoteVolumeTimer);
    remoteVolumeTimer = window.setTimeout(() => {
      const value = remoteVolume;
      remoteVolumeTimer = 0;
      // Back to following the computer once it has been told.
      void sendVolume(value).then(() => { if (!remoteVolumeTimer) remoteVolume = -1; });
    }, 160);
  }

  /** One press of a volume key: the step Android leaves to the app. */
  function adjustRemoteVolume(direction: number) {
    setRemoteVolume(remoteVolumePercent() + (direction > 0 ? 5 : -5));
  }

  /**
   * Where the computer is, as well as this phone can tell.
   *
   * The computer is asked every few seconds, so its answer alone would step
   * about rather than move. Between answers the position is carried forward
   * here, and the next answer puts it right again.
   */
  function remotePositionMs(): number {
    // Reading the tick is what makes this recompute between answers.
    void remoteTick;
    const state = remoteState;
    if (!state?.active) return 0;
    if (!state.playing || !remoteStateAt) return state.positionMs;
    const carried = state.positionMs + Math.max(0, Date.now() - remoteStateAt);
    return state.durationMs > 0 ? Math.min(carried, state.durationMs) : carried;
  }

  /**
   * Ask again the moment the track should have ended.
   *
   * The computer moves on by itself, and waiting for the next poll would leave
   * this phone showing the finished track for seconds after the next one began.
   */
  function noteRemotePlaybackTick() {
    const state = remoteState;
    if (!state?.playing || state.durationMs <= 0) return;
    if (remotePositionMs() < state.durationMs) return;
    if (remoteEndedFileId === state.fileId) return;
    remoteEndedFileId = state.fileId;
    void refreshRemote();
  }

  /** Ask the computer for a code this phone can hand to another device. */
  async function requestReadOnlyCode() {
    if (ticketBusy) return;
    ticketBusy = true;
    ticketError = '';
    try {
      readOnlyTicket = await invoke<ReadOnlyTicketOffer>('remote_read_only_ticket');
      if (!readOnlyTicket.qrSvg) {
        notice = 'Your computer drew no QR for this code. Copy it instead.';
      }
    } catch (nextError) {
      readOnlyTicket = null;
      ticketError = String(nextError);
    } finally {
      ticketBusy = false;
    }
  }

  async function copyReadOnlyCode() {
    const uri = readOnlyTicket?.uri;
    if (!uri) return;
    try {
      await navigator.clipboard.writeText(uri);
      notice = 'Read-only code copied';
    } catch {
      // A refused clipboard still leaves the code on screen to copy by hand.
      notice = uri;
    }
  }

  function ticketMinutesLeft() {
    if (!readOnlyTicket) return 0;
    return Math.max(0, Math.round((readOnlyTicket.expiresAt * 1000 - Date.now()) / 60_000));
  }

  /**
   * A cover is reported by album key, never by event id: the computer decides
   * which claim to report, and the phone cannot name a pubkey it cannot verify.
   */
  function openCoverReport(track: RemoteTrack) {
    const key = coverKey(track.artist ?? '', track.album ?? '');
    if (!key) {
      error = 'This track names no artist and album to report.';
      return;
    }
    reportKey = key;
    reportLabel = `${track.album || 'Untitled'} · ${track.artist || 'Unknown artist'}`;
    reportReason = 'spam';
    reportNote = '';
    reportError = '';
    showReport = true;
  }

  async function submitCoverReport() {
    if (reportBusy || !reportKey) return;
    reportBusy = true;
    reportError = '';
    try {
      const report = await invoke<CoverReport>('remote_report_cover', {
        key: reportKey,
        reason: reportReason,
        note: reportNote.trim()
      });
      notice = report.reportId
        ? `Report published as ${report.reportId.slice(0, 12)}…`
        : 'Report sent to Napstr';
      showReport = false;
      closeActions();
    } catch (nextError) {
      reportError = String(nextError);
    } finally {
      reportBusy = false;
    }
  }

  function nextRemoteRepeat(mode: RemoteRepeat): RemoteRepeat {
    return mode === 'off' ? 'all' : mode === 'all' ? 'one' : 'off';
  }

  function remoteRepeatLabel(mode: RemoteRepeat) {
    return mode === 'off' ? 'Off' : mode === 'all' ? 'All' : 'One track';
  }

  function openActions(track: RemoteTrack | null) {
    actionTrack = track;
    showSleepOptions = false;
    showSourceOptions = false;
    showTrackCode = false;
    showActions = true;
  }

  function closeActions() {
    showActions = false;
    showSleepOptions = false;
    showSourceOptions = false;
    showTrackCode = false;
    actionTrack = null;
  }

  /** The track URI Napstrfy clients understand, and what a track code carries. */
  function trackUri(track: RemoteTrack) {
    return `napstrfy://track/${track.fileId}`;
  }

  /**
   * Draw the code for a track's URI. The native side builds the SVG and decides
   * whether what it built is safe to insert, so a refusal is shown as text
   * rather than rendered.
   */
  async function openTrackCode(track: RemoteTrack) {
    showTrackCode = true;
    trackCodeSvg = '';
    trackCodeError = '';
    try {
      trackCodeSvg = await invoke<string>('track_code', { uri: trackUri(track) });
    } catch (nextError) {
      trackCodeError = String(nextError);
    }
  }

  async function shareTrack(track: RemoteTrack) {
    const uri = trackUri(track);
    try {
      await navigator.clipboard.writeText(uri);
      notice = `Copied ${uri}`;
    } catch {
      // A refused clipboard still leaves the link on screen to copy by hand.
      notice = uri;
    }
    closeActions();
  }

  function sleepSummary() {
    if (!sleepValue) return 'Off';
    if (sleepValue === 'track') return 'After this track';
    return sleepClock ? `${sleepClock} left` : 'Running';
  }

  /** Choosing the running option again turns the timer off. */
  function chooseSleep(option: SleepOption) {
    if (sleepValue === option.value) {
      sleepValue = '';
      sleepEndsAt = 0;
      sleepClock = '';
      return;
    }
    sleepValue = option.value;
    if (option.endsTrack) {
      sleepEndsAt = 0;
      sleepClock = '';
      // Over on the computer this phone cannot hear the track end, so the file
      // it is on now is what the timer waits to see change.
      sleepRemoteFileId = playbackTarget === 'desktop' ? remoteState?.fileId ?? '' : '';
      return;
    }
    const total = (option.minutes ?? 0) * 60;
    sleepEndsAt = Date.now() + total * 1000;
    sleepClock = `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`;
  }

  function shuffled<T>(items: T[]): T[] {
    const copy = [...items];
    for (let index = copy.length - 1; index > 0; index -= 1) {
      const swap = Math.floor(Math.random() * (index + 1));
      [copy[index], copy[swap]] = [copy[swap], copy[index]];
    }
    return copy;
  }

  function readPlayedAlbums(): PlayedAlbum[] {
    try {
      const raw = window.localStorage.getItem(musicHistoryKey);
      const parsed = raw ? (JSON.parse(raw) as PlayedAlbum[]) : [];
      return Array.isArray(parsed)
        ? parsed.filter((entry) => entry && typeof entry.key === 'string')
        : [];
    } catch {
      return [];
    }
  }

  /** Recent albums live on the phone: the host has no play history to ask for. */
  function rememberPlayedAlbum(track: RemoteTrack) {
    const key = coverKey(track.artist ?? '', track.album ?? '');
    if (!key) return;
    playedAlbums = [
      { key, artist: track.artist, album: track.album },
      ...playedAlbums.filter((entry) => entry.key !== key)
    ].slice(0, 24);
    try {
      window.localStorage.setItem(musicHistoryKey, JSON.stringify(playedAlbums));
    } catch {
      // A history that cannot be stored is only a lost convenience.
    }
  }

  /**
   * Whether a track's artist string can belong to this album's artist.
   *
   * Neither the catalogue nor the companion protocol carries an album-level
   * artist or any guest-credit structure, so the only widening an album fetch
   * may take beyond an exact `artist|album` match is a literal extension of the
   * album's own name: "ZZ Top feat. X" belongs to ZZ Top's album, "Will Smith"
   * does not.
   */
  function artistBelongsToAlbum(trackArtist: string, albumArtist: string) {
    const artist = (trackArtist ?? '').trim().toLocaleLowerCase();
    const owner = (albumArtist ?? '').trim().toLocaleLowerCase();
    if (!owner) return artist.length === 0;
    if (artist === owner) return true;
    return artist.startsWith(owner) && /^[^a-z0-9]/.test(artist.slice(owner.length));
  }

  /**
   * The shelves are grouped out of the tracks this phone has loaded, which is
   * one page of the library. An album's other tracks are usually not in it, so
   * ask the host for the album before queueing anything.
   */
  async function albumPlaylist(album: AlbumShelf): Promise<RemoteTrack[]> {
    const name = album.album.trim();
    if (!name) return album.tracks;
    try {
      const page = await invoke<LibraryPage>('remote_library', {
        query: name, offset: 0, limit: MAX_ALBUM_TRACKS
      });
      const sameAlbum = (track: RemoteTrack) =>
        (track.album ?? '').trim().toLocaleLowerCase() === name.toLocaleLowerCase();
      const byKey = page.tracks.filter(
        (track) => coverKey(track.artist ?? '', track.album ?? '') === album.key
      );
      // The album name is not an identity: "Greatest Hits" is a title almost
      // every artist has used, and matching on it alone drew ZZ Top, Linkin Park
      // and Will Smith into one album. Widening far enough for a guest credit
      // only needs to reach the album's own artist.
      const credited = page.tracks.filter(
        (track) =>
          !byKey.includes(track)
          && sameAlbum(track)
          && artistBelongsToAlbum(track.artist ?? '', album.artist)
      );
      const found = [...byKey, ...credited];
      return found.length > album.tracks.length ? found : album.tracks;
    } catch {
      // Offline, or a host that cannot answer: play what the shelf already had.
      return album.tracks;
    }
  }

  /** `MP3 · 320 kb/s`. The rate is the file's own average, not a claim. */
  function fileSummary(track: RemoteTrack): string {
    const parts: string[] = [];
    if (track.format) parts.push(track.format.toUpperCase());
    const bitrate = averageBitrate(track);
    if (bitrate > 0) parts.push(`${bitrate} kb/s`);
    return parts.join(' · ');
  }

  function averageBitrate(track: RemoteTrack): number {
    if (!track.size || duration <= 0) return 0;
    return Math.round((track.size * 8) / duration / 1000);
  }

  async function moveTrack(direction: -1 | 1) {
    if (activeMedia !== 'music' || playerQueue.length < 2) return;
    let next: number;
    if (shuffle) {
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

  /** Move one track on whichever player the drawer is showing. */
  async function moveTrackBy(direction: -1 | 1) {
    if (playbackTarget === 'desktop') {
      if (status.streamOnly || !shownCanSkip) return;
      await sendPlayback({ type: direction === 1 ? 'next' : 'previous' });
      return;
    }
    await moveTrack(direction);
  }

  function handleTrackEnded() {
    playing = false;
    syncSystemMedia(true);
    // A sleep timer set to end of track stops here rather than advancing, for
    // podcasts as much as for music.
    if (sleepValue === 'track') {
      sleepValue = '';
      return;
    }
    if (activeMedia !== 'music') return;
    if (loopMode === 'one') {
      audio.currentTime = 0;
      audio.play().catch((nextError) => (error = String(nextError)));
      return;
    }
    if (shuffle) {
      // Every track has been played and the queue is not set to repeat.
      if (randomUpcoming < 0) return;
      void moveTrack(1);
      return;
    }
    if (playerIndex >= playerQueue.length - 1 && loopMode === 'off') return;
    void moveTrack(1);
  }

  function selectChip(chip: string) {
    activeTab = 'search';
    void searchTracks(query.toLocaleLowerCase() === chip.toLocaleLowerCase() ? '' : chip);
  }

  /** Switching to the library tab drops any search that was in flight. */
  function showMusic() {
    activeTab = 'music';
    if (query.trim()) {
      void searchTracks('');
      return;
    }
    if (!showingLikedMusic) void loadLibrary();
  }

  function showSearch() {
    activeTab = 'search';
  }

  /**
   * Answer a `napstrfy://` link: pair with a computer, look an album up, or play
   * a track this phone or its computer already knows about.
   *
   * A track that neither knows is not something to swallow quietly: the link
   * names an exact file, so the search tab is opened on that id with a notice
   * saying it was looked for here and not found.
   */
  async function handleDeepLink(value: string) {
    const link = parseDeepLink(value);
    if (!link) return;
    if (link.kind === 'pair') {
      await pair(link.ticket);
      return;
    }
    if (link.kind === 'album') {
      showSearch();
      await searchTracks(link.terms);
      return;
    }
    const known = tracks.find((track) => track.fileId === link.fileId)
      ?? likedMusic.find((track) => track.fileId === link.fileId);
    if (known) {
      await activateTrack(known);
      return;
    }
    showSearch();
    await searchTracks(link.fileId);
    notice = $t('That track is not on this phone or its computer yet.');
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
      // The feed's length is only a claim until the audio reports its own.
      duration = episode.duration || 0;
      durationEstimated = true;
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

  // The language is taken from the system the first time this phone runs; the
  // native side learns it through the labels published with the media state.
  onMount(() => initializeLocale(() => osLocale()));
  // A link can be the reason the app started, and can arrive while it is already
  // running, so both are asked for. Neither is fatal: a build without the plugin
  // simply has no links to answer.
  onMount(() => {
    void getCurrent()
      .then((urls) => { for (const url of urls ?? []) void handleDeepLink(url); })
      .catch(() => {});
    let stop: (() => void) | undefined;
    void onOpenUrl((urls) => { for (const url of urls) void handleDeepLink(url); })
      .then((unlisten) => { stop = unlisten; })
      .catch(() => {});
    return () => stop?.();
  });
  $effect(() => { $locale; untrack(() => syncSystemMedia()); });
  $effect(() => { duration; untrack(() => syncSystemMedia()); });

  onMount(() => {
    void invoke<string>('client_platform').then((value) => { platform = value; }).catch(() => {});
    const clearMediaSession = setupMediaSession();
    // Older builds stored one of four mode names; newer ones store both
    // settings together. Either shape restores cleanly.
    try {
      const saved = window.localStorage.getItem(playModeKey) ?? '';
      if (saved.startsWith('{')) {
        const parsed = JSON.parse(saved) as { loop?: string; shuffle?: boolean };
        if (LOOP_MODES.includes(parsed.loop as LoopMode)) loopMode = parsed.loop as LoopMode;
        shuffle = parsed.shuffle === true;
      } else if (saved) {
        loopMode = saved === 'once' ? 'off' : saved === 'repeat' ? 'one' : saved === 'random' ? 'off' : 'all';
        shuffle = saved === 'random';
      }
    } catch {
      // An unreadable preference just means the defaults.
    }
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
    // While the computer is the source, the bar is showing its track and its
    // position, so it has to be asked what it is doing often enough to look live.
    const remoteTimer = window.setInterval(() => {
      if (!document.hidden && playbackTarget === 'desktop') void refreshRemote();
    }, 2500);
    // Between those answers the bar moves rather than stepping, and a track that
    // has run out is noticed here instead of waiting for the next poll.
    const remoteTickTimer = window.setInterval(() => {
      if (document.hidden || playbackTarget !== 'desktop') return;
      remoteTick = Date.now();
      noteRemotePlaybackTick();
      // The lock screen's progress has no audio events to ride on out here.
      syncSystemMedia();
    }, 500);
    const transferTimer = window.setInterval(() => {
      if (!document.hidden && pending.size > 0) void refreshTransfers();
    }, 3000);
    const podcastTimer = window.setInterval(() => {
      if (!document.hidden && hasActivePodcastDownload()) void refreshPodcastDownloads();
    }, 2500);
    const sleepTimer = window.setInterval(() => {
      if (!sleepEndsAt) return;
      const remaining = sleepEndsAt - Date.now();
      if (remaining <= 0) {
        finishSleepTimer();
        return;
      }
      const seconds = Math.ceil(remaining / 1000);
      sleepClock = `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
    }, 1000);
    const foreground = () => {
      if (document.hidden) return;
      void refreshStatus();
      void refreshTransfers();
      void refreshPodcastDownloads();
      // The cache can have grown or been pruned while the app was away, and a
      // prefetched track never announces that it has arrived.
      void refreshCachedIds();
      // Unlocking after a while away should show what is playing now, not what
      // was playing when the screen went off.
      if (playbackTarget === 'desktop') void refreshRemote();
      syncSystemMedia(true);
    };
    document.addEventListener('visibilitychange', foreground);
    window.addEventListener('napstrfy-media-action', handleSystemMediaAction);
    window.addEventListener('napstrfy-back', handleSystemBack);
    window.addEventListener('keydown', handleKeyboard);
    return () => {
      window.clearInterval(statusTimer);
      window.clearInterval(remoteTimer);
      window.clearInterval(remoteTickTimer);
      window.clearInterval(transferTimer);
      window.clearInterval(podcastTimer);
      window.clearInterval(sleepTimer);
      document.removeEventListener('visibilitychange', foreground);
      window.removeEventListener('napstrfy-media-action', handleSystemMediaAction);
      window.removeEventListener('napstrfy-back', handleSystemBack);
      window.removeEventListener('keydown', handleKeyboard);
      clearMediaSession();
      mediaUpdates.cancel();
      pushBackAvailability(false);
      androidMediaBridge()?.clear();
    };
  });
</script>

<svelte:head><title>Napstrfy</title></svelte:head>

{#snippet trackList(emptyTitle: string, emptyHint: string, showLoadMore: boolean)}
  <section
    class="track-list"
    class:sliding={likedSwipeActive}
    role="list"
    aria-busy={loading || searchingNetwork}
    style:transform={showingLikedMusic && likedSwipeX ? `translateX(${likedSwipeX}px)` : null}
    onpointerdown={startLikedSwipe}
    onpointermove={moveLikedSwipe}
    onpointerup={endLikedSwipe}
    onpointercancel={endLikedSwipe}
    onclickcapture={swallowLikedSwipeClick}
  >
    {#if !loading && !searchingNetwork && tracks.length === 0}
      <div class="empty-library"><img src="/napstr-logo-small.png" alt="" /><h2>{emptyTitle}</h2><p>{emptyHint}</p></div>
    {/if}
    {#each tracks as track (track.fileId)}
      <div class:selected={selected?.fileId === track.fileId} class:remote={!track.local} class:liked={isTrackLiked(track)} class="track-row" role="listitem">
        <button class="track-open" disabled={status.streamOnly && !track.local} onclick={() => activateTrack(track)}>
          <TrackArtwork {track} lookup />
          <span class="track-copy">
            <strong>{title(track)}</strong>
            <small>{artist(track)}{track.album ? ` · ${track.album}` : ''}</small>
            <span class="track-meta">{readableSize(track.size)}</span>
          </span>
          <TrackBadge {track} cached={cachedFileIds.has(track.fileId)} pending={pending.has(track.fileId)} />
        </button>
        <!-- The row's own control is the track menu rather than a heart: liking
             is one of its rows, alongside sharing and the code below, so a
             second place to press would only compete with it. -->
        <button class="track-more" onclick={() => openActions(track)} aria-label={$t("Track options")}>
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <circle class="filled" cx="12" cy="5.6" r="1.5" /><circle class="filled" cx="12" cy="12" r="1.5" /><circle class="filled" cx="12" cy="18.4" r="1.5" />
          </svg>
        </button>
      </div>
    {/each}
    {#if showLoadMore && tracks.length < total}<button class="load-more" onclick={() => loadLibrary(true)} disabled={loadingMore}>{loadingMore ? 'Loading…' : `Load more · ${tracks.length} of ${total}`}</button>{/if}
  </section>
{/snippet}

{#snippet playbackTargetRows()}
  <button class:active={playbackTarget === 'phone'} class="actions-row" onclick={() => choosePlaybackTarget('phone')}>
    <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="7" y="2.8" width="10" height="18.4" rx="2.2" /><path d="M11 18.4h2" /></svg>
    <span>{$t("This phone")}</span>
    {#if playbackTarget === 'phone'}<small>{$t("Playing here")}</small>{/if}
  </button>
  <button
    class:active={playbackTarget === 'desktop'}
    class="actions-row"
    disabled={!desktopTargetAvailable()}
    onclick={() => choosePlaybackTarget('desktop')}
  >
    <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4.4" width="18" height="12.2" rx="1.8" /><path d="M8.5 20h7" /><path d="M12 16.6V20" /></svg>
    <span>{status.desktopName || 'The computer'}</span>
    <small>
      {!status.paired
        ? 'Not paired'
        : !status.connected
          ? 'Not reachable'
          : status.streamOnly
            ? 'Read-only pairing'
            : playbackTarget === 'desktop'
              ? 'Playing there'
              : 'Play its library here'}
    </small>
  </button>
{/snippet}

{#if !status.paired && activeTab !== 'podcasts'}
  <main class="pair-screen">
    <div class="pair-glow"></div>
    <div class="pair-logo" aria-label="Napstrfy"><img src={appIcon} alt="" /><span>napstrfy</span></div>
    <p class="eyebrow">{$t("NAPSTR COMPANION")}</p>
    <h1>{$t("Your music.")}<br />{$t("Wherever you are.")}</h1>
    <p class="pair-copy">{$t("Pair securely with Napstr on your computer. Discovery and Tor downloads stay there; your music reaches this phone over encrypted Iroh.")}</p>
    {#if error}
      <div class="error-card">
        <span>{$t(error)}</span>
        {#if cameraPermissionDenied}<button onclick={showCameraSettings}>{$t("Open app settings")}</button>{/if}
      </div>
    {/if}
    {#if mobile}
      <button class="scan-button" onclick={scanCode} disabled={scanning || pairing || statusLoading}><span>▦</span>{scanning ? $t("Opening camera…") : pairing ? $t("Pairing…") : $t("Scan Napstr QR")}</button>
    {/if}
    <button class="browse-podcasts" onclick={showPodcasts}>{$t("Listen to podcasts without pairing")}</button>
    <details class="manual-pair" class:desktop-pair={!mobile} bind:open={manualPairOpen}>
      <summary>{mobile ? $t("Enter a pairing code instead") : $t("Connect with a pairing code")}</summary>
      <p>{$t("On the computer running Napstr, open")} <strong>{$t("Napstrfy → Pair without a camera")}</strong>{$t(". Copy the code and paste it here within five minutes.")}</p>
      <textarea bind:value={pairingCode} aria-label={$t("Napstr pairing code")} placeholder="napstrfy://pair/…" spellcheck="false" autocapitalize="off" autocomplete="off"></textarea>
      <button onclick={() => pair()} disabled={!pairingCode.trim() || pairing || statusLoading}>{pairing ? $t("Connecting…") : $t("Connect")}</button>
    </details>
    <LanguageSelect />
    <small class="pair-security">{$t("One-use pairing · no Nostr keys leave your computer")}</small>
  </main>
{:else}
  <main class="app-shell" class:desktop={desktopShell}>
    <header class="mobile-header">
      {#if status.paired}
        <button class="status-chip" class:offline={!status.connected} onclick={reconnect} title={status.connected ? `Connected to ${status.desktopName || 'Napstr'}` : 'Reconnect to Napstr'}>
          <i></i><span>{statusPending ? $t("Connecting…") : status.connected ? status.desktopName || 'Napstr' : $t("Offline")}{status.streamOnly ? $t(" · Read only") : ''}</span>
        </button>
      {:else}
        <button class="status-chip offline" onclick={showMusic}><i></i><span>{$t("Pair Napstr")}</span></button>
      {/if}
      <button class="header-icon" onclick={() => (showSettings = true)} aria-label={$t("Settings")}>
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M3.4 7.6h9.4" /><path d="M17.6 7.6h3" /><circle cx="15.2" cy="7.6" r="2.4" />
          <path d="M3.4 16.4h4.2" /><path d="M12.4 16.4h8.2" /><circle cx="10" cy="16.4" r="2.4" />
        </svg>
      </button>
    </header>

    <!-- On a desktop this is the middle column of the shell grid. -->
    <div class="app-content">
      {#if error}<button class="error-banner" onclick={() => (error = '')}>{$t(error)}<span>×</span></button>{/if}
      {#if notice}{#key notice}<div class="toast" role="status">{$t(notice)}</div>{/key}{/if}

      {#if activeTab === 'search'}
        <section class="search-area">
          <form onsubmit={(event) => { event.preventDefault(); event.currentTarget.querySelector('input')?.blur(); void searchTracks(); }}>
            <span>⌕</span><input bind:value={query} placeholder={status.streamOnly ? "Search Napstr’s music" : "Search your music and Nostr"} aria-label={$t("Search tracks")} />
            {#if loading || searchingNetwork}<i class="search-spinner" role="status" aria-label={$t("Searching")}></i>{/if}
            {#if query}<button type="button" class="clear-search" onclick={() => searchTracks('')}>×</button>{/if}
          </form>
        </section>
        <div class="chips-row"><div class="chips"><button class:active={showingLikedMusic} onclick={showLikedTracks}>{$t("♥ Liked")}</button>{#each musicChips as chip}<button class:active={!showingLikedMusic && query.toLocaleLowerCase() === chip.toLocaleLowerCase()} onclick={() => selectChip(chip)}>{chip}</button>{/each}</div></div>
  
        {#if searching && (resultArtists.length > 0 || libraryAlbums.length > 0)}
          <section class="album-shelves">
            {#if resultArtists.length > 0}
              <div class="album-shelf-block">
                <div class="section-label"><b>{$t("Artists")}</b><span>{resultArtists.length} {$t("in these results")}</span></div>
                <div class="album-shelf">
                  {#each resultArtists as entry (entry.name)}
                    <div class="artist-card">
                      <button class="artist-open" onclick={() => void searchTracks(entry.name)} aria-label={`Show tracks by ${entry.name}`}>
                        <TrackArtwork track={entry.representative} lookup />
                        <span class="album-play" aria-hidden="true">⌕</span>
                      </button>
                      <strong>{entry.name}</strong>
                      <small>{entry.count} {entry.count === 1 ? 'track' : 'tracks'}</small>
                    </div>
                  {/each}
                </div>
              </div>
            {/if}
            {#if libraryAlbums.length > 0}
              <div class="album-shelf-block">
                <div class="section-label"><b>{$t("Albums")}</b><span>{libraryAlbums.length} {$t("in these results")}</span></div>
                <div class="album-shelf">
                  {#each libraryAlbums as album (album.key)}
                    <div class="album-card">
                      <button class="album-open" onclick={() => void openAlbum(album)} aria-label={`Open ${album.album} by ${album.artist || 'an unknown artist'}`}>
                        <TrackArtwork track={album.representative} lookup />
                        <span class="album-play" aria-hidden="true">▶</span>
                      </button>
                      <strong>{album.album}</strong>
                      <small>{album.artist || 'Unknown artist'}</small>
                    </div>
                  {/each}
                </div>
              </div>
            {/if}
          </section>
          <div class="section-label tracks-label"><b>{$t("Tracks")}</b><span>{tracks.length} {tracks.length === 1 ? 'result' : 'results'}</span></div>
        {:else if !query.trim()}
          <section class="library-heading"><div><p>{$t("SEARCH")}</p><h1>{$t("Find something")}</h1></div><span>{$t("Your library and the network")}</span></section>
        {/if}
  
        {@render trackList(
          'No tracks found',
          query.trim() ? 'Try different words or clear the search.' : 'Search your Napstr library and the network.',
          !showingLikedMusic && Boolean(query.trim())
        )}
      {:else if activeTab === 'music'}
        <section class="library-heading">
          <div><p>{showingLikedMusic ? 'FAVOURITES' : 'YOUR NAPSTR'}</p><h1>{showingLikedMusic ? 'Liked music' : 'Your music'}</h1></div>
          {#if showingLikedMusic}
            <div class="heading-end">
              <span>{total} {total === 1 ? 'track' : 'tracks'}</span>
              <button class="liked-close" onclick={closeLikedMusic} aria-label={$t("Close liked music")}>
                <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6.5 6.5l11 11" /><path d="M17.5 6.5l-11 11" /></svg>
              </button>
            </div>
          {:else}
            <span>{total} {total === 1 ? 'track' : 'tracks'}</span>
          {/if}
        </section>
  
        {#if !showingLikedMusic && (discoverAlbums.length > 0 || lastPlayed.length > 0)}
          <section class="album-shelves">
            {#if lastPlayed.length > 0}
              <div class="album-shelf-block">
                <div class="section-label"><b>{$t("Last played")}</b><span>{$t("Recent albums")}</span></div>
                <div class="album-shelf">
                  {#each lastPlayed as album (album.key)}
                    <div class="album-card">
                      <button class="album-open" onclick={() => void openAlbum(album)} aria-label={`Open ${album.album} by ${album.artist || 'an unknown artist'}`}>
                        <TrackArtwork track={album.representative} lookup />
                        <span class="album-play" aria-hidden="true">▶</span>
                      </button>
                      <strong>{album.album}</strong>
                      <small>{album.artist || 'Unknown artist'}</small>
                    </div>
                  {/each}
                </div>
              </div>
            {/if}
            {#if discoverAlbums.length > 0}
              <div class="album-shelf-block">
                <div class="section-label"><b>{$t("Discover albums")}</b><span>{libraryAlbums.length} {$t("in this library")}</span></div>
                <div class="album-shelf">
                  {#each discoverAlbums as album (album.key)}
                    <div class="album-card">
                      <button class="album-open" onclick={() => void openAlbum(album)} aria-label={`Open ${album.album} by ${album.artist || 'an unknown artist'}`}>
                        <TrackArtwork track={album.representative} lookup />
                        <span class="album-play" aria-hidden="true">▶</span>
                      </button>
                      <strong>{album.album}</strong>
                      <small>{album.artist || 'Unknown artist'}</small>
                    </div>
                  {/each}
                </div>
              </div>
            {/if}
          </section>
        {/if}
  
        {@render trackList(
          showingLikedMusic ? 'No liked tracks yet' : 'No tracks found',
          showingLikedMusic ? 'Tap the heart beside a song to keep it here.' : 'Add music to your Napstr folder on the computer.',
          !showingLikedMusic
        )}
      {:else if activeTab === 'podcasts'}
        <section class="search-area podcast-search">
          <form onsubmit={(event) => { event.preventDefault(); event.currentTarget.querySelector('input')?.blur(); void searchPodcasts(); }}>
            <span>⌕</span><input bind:value={podcastQuery} placeholder={$t("Search podcasts")} aria-label={$t("Search podcasts")} />
            {#if podcastQuery}<button type="button" class="clear-search" onclick={() => { podcastQuery = ''; void loadTrendingPodcasts(); }}>×</button>{/if}
          </form>
        </section>
        <div class="chips-row"><div class="chips podcast-genres"><button class:active={showingLikedPodcasts} onclick={showLikedPodcastList}>{$t("♥ Liked")}</button>{#each podcastGenres as genre}<button class:active={!showingLikedPodcasts && podcastGenre === genre} onclick={() => selectPodcastGenre(genre)}>{genre}</button>{/each}</div></div>
  
        {#if selectedPodcast}
          <section class="podcast-show-heading">
            <button class="podcast-back" onclick={() => { selectedPodcast = null; podcastEpisodes = []; }}>‹</button>
            {#if selectedPodcast.image}<img src={selectedPodcast.image} alt="" />{:else}<div class="podcast-art-fallback">◉</div>{/if}
            <div><p>{$t("PODCAST")}</p><h1>{selectedPodcast.title}</h1><small>{selectedPodcast.author || 'Independent podcast'}</small></div>
            <button class:liked={isPodcastLiked(selectedPodcast)} class="like-button podcast-heading-like" onclick={() => togglePodcastLike(selectedPodcast!)} aria-label={`${isPodcastLiked(selectedPodcast) ? 'Unlike' : 'Like'} ${selectedPodcast.title}`}>{isPodcastLiked(selectedPodcast) ? '♥' : '♡'}</button>
          </section>
          <section class="episode-list" aria-busy={podcastLoading}>
            {#if podcastLoading}<div class="loading-list"><i></i><span>{$t("Loading episodes…")}</span></div>{/if}
            {#each podcastEpisodes as episode (episode.id)}
              {@const download = podcastDownloadFor(episode.id)}
              {@const episodeImage = episode.image || selectedPodcast.image}
              <article class="episode-row">
                <button class="episode-art" onclick={() => playPodcast(episode)} aria-label={`Play ${episode.title}`}>
                  <span class="podcast-art-fallback">◉</span>
                  {#if episodeImage}<img src={episodeImage} alt="" onerror={(event) => usePodcastArtwork(event, selectedPodcast!.image)} />{/if}
                  <i aria-hidden="true">▶</i>
                </button>
                <button class="episode-copy" onclick={() => playPodcast(episode)}>
                  <strong>{episode.title}</strong>
                  {#if episode.description}<span>{episode.description}</span>{/if}
                  <small>{podcastDate(episode.datePublished)}{episode.duration ? ` · ${clock(episode.duration)}` : ''}</small>
                </button>
                <button class:ready={download?.ready} class="episode-download" onclick={() => downloadPodcast(episode)} disabled={download?.status === 'Downloading'} aria-label={`Download ${episode.title}`} title={download?.status || 'Download for offline listening'}>{download?.ready ? '✓' : download?.status === 'Downloading' ? `${Math.round(download.progress)}%` : '⇩'}</button>
              </article>
            {/each}
            {#if !podcastLoading && podcastEpisodes.length === 0}<div class="empty-library"><h2>{$t("No playable episodes")}</h2><p>{$t("This feed may not currently expose supported HTTPS audio.")}</p></div>{/if}
          </section>
        {:else}
          {#if podcastHistory.length > 0 && !podcastQuery && !podcastGenre && !showingLikedPodcasts}
            <section class="podcast-history"><div class="section-label"><b>{$t("Recently played")}</b><span>{$t("Last 10")}</span></div><div class="history-scroller">{#each podcastHistory as episode (episode.id)}<button onclick={() => playPodcast(episode)}>{#if episode.image}<img src={episode.image} alt="" />{:else}<span>◉</span>{/if}<strong>{episode.title}</strong><small>{episode.feedTitle}</small></button>{/each}</div></section>
          {/if}
          <section class="library-heading">
            <div><p>{showingLikedPodcasts ? 'FAVOURITES' : 'POWERED BY PODCAST INDEX'}</p><h1>{showingLikedPodcasts ? 'Liked podcasts' : podcastGenre ? podcastGenre : podcastQuery ? `Results for “${podcastQuery}”` : 'Discover podcasts'}</h1></div>
            <span>{podcastFeeds.length} {$t("shows")}</span>
          </section>
          <section class="podcast-grid" aria-busy={podcastLoading}>
            {#if podcastLoading}<div class="loading-list"><i></i><span>{$t("Searching podcasts…")}</span></div>{/if}
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
          <form onsubmit={(event) => { event.preventDefault(); event.currentTarget.querySelector('input')?.blur(); void loadAudiobooks(); }}>
            <span>⌕</span><input bind:value={audiobookQuery} placeholder={$t("Search audiobooks")} aria-label={$t("Search audiobooks")} />
            {#if audiobookQuery}<button type="button" class="clear-search" onclick={() => { audiobookQuery = ''; void loadAudiobooks(); }}>×</button>{/if}
          </form>
        </section>
  
        {#if selectedAudiobook}
          <section class="audiobook-show-heading">
            <button class="podcast-back" onclick={() => (selectedAudiobook = null)}>‹</button>
            <div class="audiobook-cover">▥</div>
            <div><p>{$t("AUDIOBOOK")}</p><h1>{selectedAudiobook.title}</h1><small>{selectedAudiobook.author || 'Unknown author'}{selectedAudiobook.narrator ? ` · Read by ${selectedAudiobook.narrator}` : ''}</small></div>
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
            <span>{audiobookTotal} {audiobookTotal === 1 ? 'book' : 'books'}</span>
          </section>
          <section class="audiobook-list" aria-busy={audiobookLoading}>
            {#if audiobookLoading}<div class="loading-list"><i></i><span>{$t("Asking Napstr…")}</span></div>{/if}
            {#each audiobooks as book (book.audiobookId)}
              <button class="audiobook-card" onclick={() => openAudiobook(book)}>
                <span class="audiobook-cover">▥</span>
                <span><strong>{book.title}</strong><small>{book.author || 'Unknown author'}</small><i>{book.chapterCount} {book.chapterCount === 1 ? 'file' : 'chapters'} · {readableSize(book.totalSize)}</i></span>
                <b>›</b>
              </button>
            {/each}
            {#if !audiobookLoading && audiobooks.length === 0}<div class="empty-library"><h2>{$t("No audiobooks found")}</h2><p>{$t("Group a chapter folder in Napstr, or add the tag “audiobook” to a complete one-file book.")}</p></div>{/if}
          </section>
        {/if}
      {/if}
    </div>

    <nav class:dragging={sheetDragging || barDragging} style={`--nav-shift:${navShift}`} class="bottom-nav" aria-label={$t("Napstrfy navigation")}>
      <button class:active={activeTab === 'music'} onclick={showMusic}><span>♫</span>{$t("Music")}</button>
      <button class:active={activeTab === 'search'} onclick={showSearch}><span class="nav-icon"><svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="6.4" /><path d="M15.9 15.9 20.6 20.6" /></svg></span>{$t("Search")}</button>
      <button class:active={activeTab === 'podcasts'} onclick={showPodcasts}><span>◉</span>{$t("Podcasts")}</button>
      <button class:active={activeTab === 'audiobooks'} onclick={showAudiobooks}><span>▥</span>{$t("Audiobooks")}</button>
    </nav>

    {#if !pinned}
      <!-- The compact bar is the phone's player; a pinned desktop column replaces it. -->
      <section
        class:dragging={barDragging}
        style={`--bar-shift:${barShift}px; --bar-opacity:${barFade}; --bar-progress:${barProgress}; ${barArtStyle}`}
        class:empty={barEmpty}
        class="now-playing"
      >
        <span class="now-fill" aria-hidden="true"></span>
        <button
          class="now-open"
          bind:this={barElement}
          disabled={!nowPlayingAvailable()}
          onclick={handleBarClick}
          onpointerdown={startBarDrag}
          onpointermove={moveBarDrag}
          onpointerup={endBarDrag}
          onpointercancel={endBarDrag}
          aria-label={playbackTarget === 'desktop' ? `Open what ${playbackTargetLabel()} is playing` : 'Open the now playing screen'}
        >
          {#if playbackTarget === 'desktop'}
            {#if barTrack}<TrackArtwork track={barTrack} large lookup />{:else}<div class="empty-art">♬</div>{/if}
          {:else if activeMedia === 'podcast' && currentPodcast}
            {#if currentPodcast.image}<img class="podcast-player-art" src={currentPodcast.image} alt="" />{:else}<div class="empty-art">◉</div>{/if}
          {:else if current}<TrackArtwork track={current} large lookup />{:else}<div class="empty-art">♪</div>{/if}
          <div class="now-copy">
            <div class="now-title" bind:this={titleClipper}>
              {#key nowTitle}
                <div class:marquee={titleOverflows} class="now-title-row">
                  <span bind:this={titleText}>{nowTitle}</span>
                  {#if titleOverflows}<span aria-hidden="true">{nowTitle}</span>{/if}
                </div>
              {/key}
            </div>
            <small>{nowArtist}</small>
          </div>
        </button>
        <button class="now-play" onclick={togglePlayer} disabled={barEmpty || (playbackTarget !== 'desktop' && caching)} aria-label={barPlaying ? 'Pause' : 'Play'}>
          {#if playbackTarget !== 'desktop' && caching}<span class="icon-busy"></span>{:else if barPlaying}<span class="icon-pause"></span>{:else}<span class="icon-play"></span>{/if}
        </button>
      </section>
    {/if}

    {#if showNowPlaying || pinned}
      <div
        class="now-sheet"
        class:pinned
        class:entering={sheetEntering}
        class:closing={sheetClosing}
        class:dragging={sheetDragging}
        bind:this={sheetElement}
        style={`--sheet-drag:${sheetDragY}px; --cover-hue:${artworkHue(shownTrack?.fileId ?? '')}`}
        role={pinned ? 'complementary' : 'dialog'}
        aria-modal={pinned ? undefined : 'true'}
        tabindex="-1"
        aria-label={$t("Now playing")}
        onpointerdown={startSheetDrag}
        onpointermove={moveSheetDrag}
        onpointerup={endSheetDrag}
        onpointercancel={endSheetDrag}
      >
        <div class="now-sheet-hero">
          <div
            class="now-sheet-backdrop"
            class:empty={!sheetThumbUrl() && !sheetCoverUrl()}
            style={sheetThumbUrl() || sheetCoverUrl() ? `background-image:url(${sheetThumbUrl() || sheetCoverUrl()})` : ''}
          ></div>
          <div class="now-sheet-scrim"></div>
          <div class="now-sheet-top">
            <button class="now-sheet-icon now-sheet-close" onclick={closeNowPlaying} aria-label={$t("Close the now playing screen")}>
              <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 9.5 12 16l7-6.5" /></svg>
            </button>
            <div class="now-sheet-top-buttons">
              {#if status.paired}
                <button class="now-sheet-icon" onclick={openSourcePicker} aria-label={`Play on: ${playbackTargetLabel()}`} title={$t("Play on")}>
                  <svg viewBox="0 0 24 24" aria-hidden="true">
                    <rect x="3.4" y="5.2" width="17.2" height="12.6" rx="2" />
                    <circle class="filled" cx="4.9" cy="16.9" r="1.1" />
                    <path d="M7.4 16.9A2.5 2.5 0 0 0 4.9 14.4" /><path d="M9.6 16.9A4.7 4.7 0 0 0 4.9 12.2" />
                  </svg>
                </button>
              {/if}
              <button class="now-sheet-icon" onclick={() => openActions(null)} aria-label={$t("Track options")}>
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <circle class="filled" cx="12" cy="5.6" r="1.7" /><circle class="filled" cx="12" cy="12" r="1.7" /><circle class="filled" cx="12" cy="18.4" r="1.7" />
                </svg>
              </button>
            </div>
          </div>
          <div class="now-sheet-art">
            {#if sheetThumbUrl()}
              <!-- The tile that was tapped has this one already, so the square is
                   a cover the moment the drawer opens. -->
              <img class="now-sheet-art-thumb" src={sheetThumbUrl()} alt="" aria-hidden="true" />
            {/if}
            <!-- A publisher who gave one rendition gave nothing to fade to, and
                 the thumbnail is that same image, so it is the only layer. -->
            {#if sheetCoverUrl() && sheetCoverUrl() !== sheetThumbUrl()}
              {@const art = sheetCoverUrl()}
              <img
                class="now-sheet-art-full"
                class:ready={sheetArtLoaded === art}
                src={art}
                alt=""
                decoding="async"
                onload={() => (sheetArtLoaded = art)}
                onerror={() => (nowArtFailed = true)}
              />
            {:else if !sheetThumbUrl()}
              <div class="now-sheet-art-empty">♪</div>
            {/if}
          </div>
        </div>

        <div class="now-sheet-body" bind:this={sheetScroller}>
          {#if playbackTarget === 'desktop' && (remoteError || remoteState?.error)}
            <p class="sheet-error">{remoteState?.error || remoteError}</p>
          {/if}

          <div class="now-sheet-timeline">
            <input
              type="range"
              min="0"
              max={shownDuration || 0}
              step="0.1"
              value={shownPosition}
              oninput={(event) => { if (playbackTarget !== 'desktop') void seekShown(Number(event.currentTarget.value)); }}
              onchange={(event) => { if (playbackTarget === 'desktop') void seekShown(Number(event.currentTarget.value)); }}
              disabled={!shownCanSeek}
              aria-label={$t("Seek")}
            />
          </div>

          <div class="now-sheet-meta">
            <span>{clock(shownPosition)}</span>
            <span class="now-sheet-speed">
              {playbackTarget === 'desktop'
                ? `${remoteState?.queueLen ?? 0} ${(remoteState?.queueLen ?? 0) === 1 ? 'track' : 'tracks'} there`
                : 'Speed: 1x'}
            </span>
            <span>{shownDurationLabel}</span>
          </div>

          {#if playbackTarget === 'desktop' && !remoteState?.active}
            <div class="now-sheet-copy">
              <h1>{$t("Nothing is playing there")}</h1>
              <p>
                {status.streamOnly
                  ? 'This pairing is read only, so it cannot start anything.'
                  : 'Napstr will pick up from wherever the computer left off.'}
              </p>
            </div>
          {:else if shownTrack}
            <div class="now-sheet-copy">
              <h1>{title(shownTrack)}</h1>
              <p>{artist(shownTrack)}</p>
              {#if shownTrack.album}<small>{shownTrack.album}</small>{/if}
              {#if playbackTarget !== 'desktop' && fileSummary(shownTrack)}<em>{fileSummary(shownTrack)}</em>{/if}
            </div>
          {/if}

          <div class="now-sheet-actions">
            <button onclick={() => void moveTrackBy(-1)} disabled={!shownCanSkip || (playbackTarget === 'phone' && shuffle && randomHistoryIndex <= 0)} aria-label={$t("Previous track")}>|◀</button>
            <button class="skip-button" onclick={() => void nudgeShown(-15)} disabled={!shownCanSeek} aria-label={$t("Back 15 seconds")} title={$t("Back 15 seconds")}>
              <SeekIcon />
            </button>
            <button class="play-main" class:square={shownPlaying} onclick={togglePlayer} disabled={playbackTarget === 'desktop' ? remoteBusy || status.streamOnly : caching} aria-label={shownPlaying ? 'Pause' : 'Play'}>
              {#if playbackTarget === 'desktop' ? remoteBusy : caching}<span class="icon-busy"></span>{:else if shownPlaying}<span class="icon-pause"></span>{:else}<span class="icon-play"></span>{/if}
            </button>
            <button class="skip-button" onclick={() => void nudgeShown(15)} disabled={!shownCanSeek} aria-label={$t("Forward 15 seconds")} title={$t("Forward 15 seconds")}>
              <SeekIcon forward />
            </button>
            <button onclick={() => void moveTrackBy(1)} disabled={!shownCanSkip} aria-label={$t("Next track")}>▶|</button>
          </div>

          {#if playbackTarget === 'desktop' && remoteState?.active}
            <label class="sheet-volume">
              <span>{$t("Volume")}</span>
              <input
                type="range"
                min="0"
                max="100"
                step="1"
                value={remoteVolumePercent()}
                oninput={(event) => setRemoteVolume(Number(event.currentTarget.value))}
                disabled={status.streamOnly}
                aria-label={$t("Volume on the computer")}
              />
            </label>
          {/if}

          <div class="now-sheet-modes">
            <button class:active={shownLoopActive} onclick={cycleShownRepeat} aria-label={shownLoopLabel} title={shownLoopLabel}>
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M4.5 9.2A4.7 4.7 0 0 1 9.2 4.5H18" /><path d="M15.6 1.8 18.6 4.5 15.6 7.2" />
                <path d="M19.5 14.8a4.7 4.7 0 0 1-4.7 4.7H6" /><path d="M8.4 22.2 5.4 19.5 8.4 16.8" />
                {#if shownLoopOne}<path d="M11.7 11.4 12.9 10.3v5.2" /><path d="M11.2 15.5h3.4" />{/if}
              </svg>
            </button>
            <button class:active={shownShuffle} onclick={toggleShownShuffle} aria-label={shownShuffle ? 'Shuffle on' : 'Shuffle off'} title={$t("Shuffle")}>
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M3.5 6.5h3.2l10.1 11h4" /><path d="M18.3 3.7 21 6.5l-2.7 2.8" />
                <path d="M3.5 17.5h3.2l10.1-11h4" /><path d="M18.3 14.7 21 17.5l-2.7 2.8" />
              </svg>
            </button>
            <button onclick={() => (showQueue = true)} disabled={playbackTarget === 'desktop' && remoteQueue.length === 0} aria-label={$t("Open the playlist")} title={$t("Playlist")}>
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M4 6.5h16" /><path d="M4 12h16" /><path d="M4 17.5h16" />
              </svg>
            </button>
            <button disabled aria-label={$t("Smart playlists, coming soon")} title={$t("Smart playlists, coming soon")}>
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M12 12h.01" /><path d="M8.4 8.4a5.1 5.1 0 0 0 0 7.2" /><path d="M15.6 8.4a5.1 5.1 0 0 1 0 7.2" />
              </svg>
            </button>
            <button class="now-mode-like" class:liked={shownLiked} aria-pressed={shownLiked} onclick={toggleShownLike} aria-label={$t("Like this track")} disabled={!shownTrack}>
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M12 20.3c-1.4-1-7.2-5.2-7.2-9.4A4.2 4.2 0 0 1 12 8.2a4.2 4.2 0 0 1 7.2 2.7c0 4.2-5.8 8.4-7.2 9.4z" />
              </svg>
            </button>
          </div>
        </div>
      </div>
    {/if}
  </main>
{/if}

{#if showQueue && (playbackTarget === 'desktop' || activeMedia === 'music')}
  <div class="queue-view" class:desktop={desktopShell} role="dialog" aria-modal="true" aria-label={$t("Playlist")}>
    <header class="queue-head">
      <div>
        <p>{playbackTarget === 'desktop' ? `ON ${(status.desktopName || 'the computer').toUpperCase()}` : shuffle ? 'SHUFFLED' : 'PLAYING NEXT'}</p>
        <h1>{shownQueue.length === 1 ? '1 track' : `${shownQueue.length} tracks`}</h1>
      </div>
      <button class="queue-close" onclick={() => (showQueue = false)} aria-label={$t("Close the playlist")}>×</button>
    </header>
    <div class="queue-list">
      {#each shownQueue as track, index (track.fileId)}
        <div class:playing={index === shownQueueIndex} class:liked={isTrackLiked(track)} class="queue-row">
          <button class="queue-open" onclick={() => void playQueueRow(index)}>
            <span class="queue-index">{index === shownQueueIndex ? '▶' : index + 1}</span>
            <TrackArtwork track={track} lookup={index < 12} />
            <span class="queue-copy"><strong>{title(track)}</strong><small>{artist(track)}</small></span>
          </button>
          <!-- Where the track is held, as on the library rows: the menu is the
               place a download would be asked for, and the badge is what says
               whether one is needed. -->
          <TrackBadge {track} cached={cachedFileIds.has(track.fileId)} pending={pending.has(track.fileId)} />
          <button class="track-more" onclick={() => openActions(track)} aria-label={$t("Track options")}>
            <svg viewBox="0 0 24 24" aria-hidden="true">
              <circle class="filled" cx="12" cy="5.6" r="1.5" /><circle class="filled" cx="12" cy="12" r="1.5" /><circle class="filled" cx="12" cy="18.4" r="1.5" />
            </svg>
          </button>
        </div>
      {/each}
      {#if shownQueue.length === 0}<p class="queue-empty">{$t("Nothing is queued yet.")}</p>{/if}
      {#if playbackTarget === 'desktop' && shownQueue.length > 0 && shownQueueIndex < 0}
        <p class="queue-note">{$t("This is the list this phone sent. The computer is playing something else now.")}</p>
      {/if}
    </div>
  </div>
{/if}

{#if showSettings}
  <div class="settings-view" role="dialog" aria-modal="true" aria-label={$t("Settings")}>
    <header class="view-head">
      <h1>{$t("Settings")}</h1>
      <button class="view-icon" onclick={() => (showSettings = false)} aria-label={$t("Close settings")}>
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6.5 6.5 17.5 17.5" /><path d="M17.5 6.5 6.5 17.5" /></svg>
      </button>
    </header>
    <div class="settings-scroll">
      <div class="settings-status">
        <i class:offline={!status.connected}></i>
        <div>
          <strong>{status.paired ? (status.connected ? status.desktopName || 'Napstr' : 'Not reachable') : 'Not paired'}</strong>
          <small>{status.paired ? (status.connected ? 'Connected over Iroh' : 'Tap reconnect to try again') : 'Pair with Napstr on your computer'}</small>
        </div>
      </div>
      {#if status.streamOnly}<p class="settings-note">{$t("This pairing is read only: it can browse and play, but cannot ask Napstr to download or publish.")}</p>{/if}

      <!-- Near the top, so the language can be changed without scrolling. -->
      <div class="settings-section">
        <LanguageSelect />
      </div>

      {#if status.paired}
        <button class="settings-row" onclick={() => { showSettings = false; void reconnect(); }} disabled={statusPending}>
          <span>{$t("Reconnect")}</span><small>{statusPending ? 'Trying…' : 'Refresh the connection now'}</small>
        </button>
        <button class="settings-row danger" onclick={() => { showSettings = false; void forgetDesktop(); }}>
          <span>{$t("Disconnect this phone")}</span><small>{$t("You will need a new QR code")}</small>
        </button>
      {:else}
        <button class="settings-row" onclick={() => { showSettings = false; showMusic(); }}>
          <span>{$t("Pair Napstr")}</span><small>{$t("Scan a QR code from the computer")}</small>
        </button>
      {/if}

      {#if status.paired}
        <div class="settings-section">
          <p>{$t("Playback")}</p>
          <button class="settings-row" onclick={openSourcePickerAlone}>
            <span>{$t("Play on")}</span>
            <small>{playbackTargetLabel()}</small>
          </button>
        </div>
      {/if}

      {#if status.paired && !status.streamOnly}
        <div class="settings-section">
          <p>{$t("Lend access")}</p>
          <button class="settings-row" onclick={() => void requestReadOnlyCode()} disabled={ticketBusy || !status.connected}>
            <span>{$t("Create a read-only code")}</span>
            <small>{ticketBusy ? 'Asking Napstr…' : 'For a guest phone'}</small>
          </button>
          {#if ticketError}<p class="settings-note">{ticketError}</p>{/if}
          {#if readOnlyTicket}
            <div class="ticket-card">
              {#if readOnlyTicket.qrSvg}
                <div class="ticket-qr">{@html readOnlyTicket.qrSvg}</div>
              {:else}
                <p class="ticket-note">{$t("Your computer drew no QR image. Send the code below instead.")}</p>
              {/if}
              <code>{readOnlyTicket.uri}</code>
              <small>{$t("Expires in about")} {ticketMinutesLeft()} {$t("minutes. Whoever scans this can browse, listen and keep what they play, and nothing else.")}</small>
              <div class="ticket-actions">
                <button onclick={() => void copyReadOnlyCode()}>{$t("Copy code")}</button>
                <button onclick={() => (readOnlyTicket = null)}>{$t("Done")}</button>
              </div>
            </div>
          {/if}
        </div>
      {/if}

      {#if COVER_DEBUG}
        <div class="settings-section">
          <p>{$t("Developer tools")}</p>
          <CoverDebug {tracks} {status} embedded />
        </div>
      {/if}
    </div>
  </div>
{/if}

{#if showAlbumView && albumView}
  <div class="album-view" class:desktop={desktopShell} style={`--cover-hue:${artworkHue(albumView.tracks[0]?.fileId ?? albumView.key)}`} role="dialog" aria-modal="true" aria-label={`${albumView.album} by ${albumView.artist}`}>
    <div class="album-glow" style={albumGlow ? `background-image:url(${albumGlow})` : ''}></div>
    <div class="album-glow-scrim"></div>
    <header class="view-head">
      <button class="view-icon" onclick={closeAlbumView} aria-label={$t("Close the album")}>
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14.5 5 8 12l6.5 7" /></svg>
      </button>
      <button class="view-icon" onclick={() => openActions(albumView?.tracks[0] ?? null)} aria-label={$t("Album options")} disabled={albumView.tracks.length === 0}>
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <circle class="filled" cx="12" cy="5.6" r="1.7" /><circle class="filled" cx="12" cy="12" r="1.7" /><circle class="filled" cx="12" cy="18.4" r="1.7" />
        </svg>
      </button>
    </header>

    <div class="album-scroll">
      <div class="album-art">
        {#if albumView.thumb}
          <!-- The shelf tile has already fetched this one, so the header paints
               at once and the full cover fades in over it. -->
          <img
            class="album-art-backdrop"
            src={albumView.thumb}
            alt=""
            aria-hidden="true"
          />
        {/if}
        {#if albumView.art && albumView.art !== albumView.thumb}
          {@const art = albumView.art}
          <img
            class="album-art-full"
            class:ready={albumArtLoaded === art}
            src={art}
            alt=""
            decoding="async"
            onload={() => (albumArtLoaded = art)}
          />
        {:else if !albumView.thumb}
          <div class="album-art-empty">♪</div>
        {/if}
      </div>

      <div class="album-title-row">
        <div class="album-title-copy">
          <h1>{albumView.album}</h1>
          <p>{albumView.artist || 'Unknown artist'}</p>
          <p class="album-meta">{$t("Album ·")} {albumView.year || 'Year unknown'}</p>
        </div>
        <button class="album-play-all" onclick={() => void playAlbumNow()} disabled={albumView.tracks.length === 0} aria-label={`Play ${albumView.album}`}>
          {#if caching}<span class="icon-busy"></span>{:else}<span class="icon-play"></span>{/if}
        </button>
      </div>

      <ol class="album-tracks">
        {#each albumView.tracks as track, index (track.fileId)}
          <li class:playing={current?.fileId === track.fileId} class:liked={isTrackLiked(track)}>
            <button class="album-track" onclick={() => void playAlbumTrack(index)}>
              <span class="album-track-index">{current?.fileId === track.fileId ? '▶' : index + 1}</span>
              <span class="album-track-copy"><strong>{title(track)}</strong><small>{artist(track)}</small></span>
            </button>
            <button class="album-track-more" onclick={() => openActions(track)} aria-label={`Options for ${title(track)}`}>
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <circle class="filled" cx="12" cy="5.6" r="1.5" /><circle class="filled" cx="12" cy="12" r="1.5" /><circle class="filled" cx="12" cy="18.4" r="1.5" />
              </svg>
            </button>
          </li>
        {/each}
      </ol>
      {#if albumView.tracks.length === 0}<p class="queue-empty">{$t("No tracks for this album yet.")}</p>{/if}

      {#if albumView.more.length > 0}
        <div class="album-shelf-block">
          <div class="section-label"><b>{$t("More by")} {albumView.artist}</b><span>{albumView.more.length} {$t("albums")}</span></div>
          <div class="album-shelf">
            {#each albumView.more as album (album.key)}
              <div class="album-card">
                <button class="album-open" onclick={() => void openAlbum(album)} aria-label={`Open ${album.album}`}>
                  <TrackArtwork track={album.representative} lookup />
                  <span class="album-play" aria-hidden="true">▶</span>
                </button>
                <strong>{album.album}</strong>
                <small>{album.artist || 'Unknown artist'}</small>
              </div>
            {/each}
          </div>
        </div>
      {/if}
    </div>
  </div>
{/if}

{#if showReport}
  <div class="report-view" role="dialog" aria-modal="true" aria-label={$t("Report a cover")}>
    <button class="actions-scrim" onclick={() => (showReport = false)} aria-label={$t("Close the report")}></button>
    <div class="actions-panel report-panel">
      <div class="report-head">
        <h1>{$t("Report this cover")}</h1>
        <p>{reportLabel}</p>
      </div>
      <div class="actions-divider"></div>
      {#each reportReasons as reason (reason.value)}
        <button class:active={reportReason === reason.value} class="actions-row" onclick={() => (reportReason = reason.value)}>
          <span>{reason.label}</span>
          {#if reportReason === reason.value}<small>{$t("Chosen")}</small>{/if}
        </button>
      {/each}
      <label class="report-note">
        <span>{$t("Anything to add? (optional)")}</span>
        <textarea bind:value={reportNote} rows="3" maxlength="500" placeholder={$t("Say what is wrong with this cover")}></textarea>
      </label>
      {#if reportError}<p class="report-error">{reportError}</p>{/if}
      <p class="remote-note">
        {$t("Signed by")} {status.desktopName || 'your computer'} {$t("as a NIP-56 report. It tells other clients which cover to distrust.")}
      </p>
      <button class="report-send" onclick={() => void submitCoverReport()} disabled={reportBusy || !status.connected}>
        {reportBusy ? 'Sending…' : 'Send report'}
      </button>
    </div>
  </div>
{/if}

{#if showSourceOptions && !showActions}
  <div class="actions-view" role="dialog" aria-modal="true" aria-label={$t("Where to play")}>
    <button class="actions-scrim" onclick={() => (showSourceOptions = false)} aria-label={$t("Close the source picker")}></button>
    <div class="actions-panel">
      <div class="actions-head">
        <div class="actions-head-copy">
          <strong>{$t("Play on")}</strong>
          <small>{$t("Where tapping a track sends it")}</small>
        </div>
      </div>
      <div class="actions-divider"></div>
      {@render playbackTargetRows()}
    </div>
  </div>
{/if}

{#if showActions && menuTrack}
  <div class="actions-view" role="dialog" aria-modal="true" aria-label={$t("Track options")}>
    <button class="actions-scrim" onclick={closeActions} aria-label={$t("Close the track options")}></button>
    <div class="actions-panel">
      <div class="actions-head">
        <TrackArtwork track={menuTrack} lookup />
        <div class="actions-head-copy"><strong>{title(menuTrack)}</strong><small>{artist(menuTrack)}</small></div>
      </div>
      <div class="actions-divider"></div>

      {#if showSleepOptions}
        <button class="actions-row back" onclick={() => (showSleepOptions = false)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14.5 5 8 12l6.5 7" /></svg>
          <span>{$t("Sleep timer")}</span><small>{sleepSummary()}</small>
        </button>
        {#each SLEEP_OPTIONS as option (option.value)}
          <button class:active={sleepValue === option.value} class="actions-row" onclick={() => chooseSleep(option)}>
            <span>{option.label}</span>
            {#if sleepValue === option.value}<small>{$t("On")}</small>{/if}
          </button>
        {/each}
      {:else if showSourceOptions}
        <button class="actions-row back" onclick={() => (showSourceOptions = false)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14.5 5 8 12l6.5 7" /></svg>
          <span>{$t("Play on")}</span><small>{playbackTargetLabel()}</small>
        </button>
        {@render playbackTargetRows()}
      {:else if showTrackCode}
        <button class="actions-row back" onclick={() => (showTrackCode = false)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14.5 5 8 12l6.5 7" /></svg>
          <span>{$t("Show Napstrfy Code")}</span>
        </button>
        <div class="actions-code">
          <img src="/napstr-logo-small.png" alt="" />
          {#if trackCodeSvg}
            <div class="actions-code-qr">{@html trackCodeSvg}</div>
          {:else if trackCodeError}
            <p class="error">{trackCodeError}</p>
          {/if}
          <small>{trackUri(menuTrack)}</small>
        </div>
      {:else}
        <button class="actions-row" onclick={() => void shareTrack(menuTrack)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 16V4" /><path d="M8 7.5 12 3.5l4 4" /><path d="M5 14v6h14v-6" /></svg>
          <span>{$t("Share")}</span><small>{trackUri(menuTrack)}</small>
        </button>
        <button class="actions-row" onclick={() => void openTrackCode(menuTrack)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3.6" y="3.6" width="6.4" height="6.4" rx="1.2" /><rect x="14" y="3.6" width="6.4" height="6.4" rx="1.2" /><rect x="3.6" y="14" width="6.4" height="6.4" rx="1.2" /><path d="M14 14h2.6v2.6H14z" /><path d="M17.8 18.4h2.6v2H17.8z" /><path d="M14 20.4h1.6" /><path d="M20.4 14v2.6" /></svg>
          <span>{$t("Show Napstrfy Code")}</span>
        </button>
        <button class="actions-row" onclick={() => toggleTrackLike(menuTrack)}>
          <svg class:filled={isTrackLiked(menuTrack)} viewBox="0 0 24 24" aria-hidden="true"><path d="M12 20.3c-1.4-1-7.2-5.2-7.2-9.4A4.2 4.2 0 0 1 12 8.2a4.2 4.2 0 0 1 7.2 2.7c0 4.2-5.8 8.4-7.2 9.4z" /></svg>
          <span>{isTrackLiked(menuTrack) ? 'Remove from Liked Songs' : 'Add to Liked Songs'}</span>
        </button>
        <button class="actions-row" disabled>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 6.5h11" /><path d="M4 12h11" /><path d="M4 17.5h7" /><path d="M17 14v6" /><path d="M14 17h6" /></svg>
          <span>{$t("Add to playlist")}</span><small>{$t("Coming soon")}</small>
        </button>
        <button class="actions-row" onclick={() => void goToAlbum(menuTrack)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="8" /><circle class="filled" cx="12" cy="12" r="2.4" /></svg>
          <span>{$t("Go to album")}</span>
        </button>
        <button class="actions-row" disabled={!menuTrack.artist.trim()} onclick={() => goToArtist(menuTrack.artist)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="8.2" r="3.4" /><path d="M5.6 19.6a6.5 6.5 0 0 1 12.8 0" /></svg>
          <span>{$t("Go to artist")}</span>
        </button>
        <button class="actions-row" disabled={!remoteAvailable()} onclick={() => openCoverReport(menuTrack)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4.8 21 20H3z" /><path d="M12 10.6v4" /><circle class="filled" cx="12" cy="17.4" r="0.9" /></svg>
          <span>{$t("Report this cover")}</span><small>{remoteAvailable() ? '' : 'Needs a connection'}</small>
        </button>
        <button class="actions-row" onclick={openSourcePicker}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3.4" y="5.2" width="17.2" height="12.6" rx="2" /><circle class="filled" cx="4.9" cy="16.9" r="1.1" /><path d="M7.4 16.9A2.5 2.5 0 0 0 4.9 14.4" /><path d="M9.6 16.9A4.7 4.7 0 0 0 4.9 12.2" /></svg>
          <span>{$t("Play on")}</span><small>{playbackTargetLabel()}</small>
        </button>
        <button class="actions-row" onclick={() => (showSleepOptions = true)}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="8" /><path d="M12 7.4V12l3.1 2" /></svg>
          <span>{$t("Sleep timer")}</span><small>{sleepSummary()}</small>
        </button>
      {/if}
    </div>
  </div>
{/if}

<audio
  bind:this={audio}
  onplay={() => { playing = true; syncSystemMedia(true); }}
  onpause={() => { playing = false; syncSystemMedia(true); }}
  ontimeupdate={() => { currentTime = audio.currentTime; syncSystemMedia(); }}
  ondurationchange={() => {
    const reported = validDuration(audio.duration);
    // A length beyond any real track is a broken reading, not a very long one,
    // and a zero means nothing is known yet: neither should displace the feed's
    // own estimate. A usable reading is the audio's answer and replaces it.
    if (reported > 0) { duration = reported; durationEstimated = false; }
    syncSystemMedia(true);
  }}
  onended={handleTrackEnded}
  onerror={() => {
    if (activeMedia === 'podcast' && currentPodcast) error = `This phone could not play ${currentPodcast.title}.`;
    else if (current) error = `This phone could not decode ${current.format} audio.`;
  }}
></audio>
