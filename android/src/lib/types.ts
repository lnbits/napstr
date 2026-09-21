export type RemoteSource = { pubkey: string; displayName: string };

export type RemoteTrack = {
  fileId: string;
  filename: string;
  title: string;
  artist: string;
  album: string;
  format: string;
  mime: string;
  size: number;
  tags: string;
  local: boolean;
  sources: RemoteSource[];
};

export type RemoteAudiobook = {
  audiobookId: string;
  title: string;
  author: string;
  narrator: string;
  totalSize: number;
  chapters: RemoteTrack[];
};

export type RemoteAudiobookSummary = Omit<RemoteAudiobook, 'chapters'> & {
  chapterCount: number;
};

export type AudiobookLibraryPage = {
  audiobooks: RemoteAudiobookSummary[];
  total: number;
};

export type CompanionStatus = {
  streamOnly: boolean;
  paired: boolean;
  connected: boolean;
  desktopName: string;
  endpointId: string;
  libraryRevision: number;
  /** Moves when the host's album art changes, so cached covers are re-asked. */
  coverRevision: number;
  error: string;
};

export type LibraryPage = { tracks: RemoteTrack[]; total: number };
export type CachedAudio = { url: string; track: RemoteTrack };

export type PodcastFeed = {
  id: number;
  title: string;
  author: string;
  description: string;
  feedUrl: string;
  image: string;
  language: string;
  episodeCount: number;
  genres: string[];
};

export type PodcastEpisode = {
  id: number;
  feedId: number;
  feedTitle: string;
  title: string;
  description: string;
  enclosureUrl: string;
  enclosureType: string;
  enclosureLength: number;
  datePublished: number;
  duration: number;
  image: string;
};

export type PodcastDownload = {
  episode: PodcastEpisode;
  progress: number;
  status: string;
  ready: boolean;
};
export type RemoteTransfer = { id: string; fileId: string; filename: string; size: number; progress: number; status: string; speed: string };

/** Repeating is a choice of three, matching the drawer and the desktop. */
export type RemoteRepeat = 'off' | 'all' | 'one';

/**
 * One transport instruction for the computer's own player. The phone never
 * plays the desktop's audio, so every one of these can still be refused.
 */
export type PlaybackCommand =
  | { type: 'play' }
  | { type: 'pause' }
  | { type: 'toggle' }
  | { type: 'stop' }
  | { type: 'next' }
  | { type: 'previous' }
  | { type: 'seek'; positionMs: number }
  | { type: 'volume'; percent: number }
  | { type: 'repeat'; mode: RemoteRepeat }
  | { type: 'shuffle'; enabled: boolean }
  /** Play one track, with the list the phone was showing as the queue. */
  | { type: 'playTrack'; fileId: string; queue: string[] };

export type RemotePlaybackState = {
  active: boolean;
  playing: boolean;
  fileId: string;
  title: string;
  artist: string;
  album: string;
  positionMs: number;
  durationMs: number;
  volume: number;
  queueLen: number;
  queueIndex: number;
  repeat: RemoteRepeat;
  shuffle: boolean;
  /** True while a phone has driven the host recently. */
  remoteControl: boolean;
  error: string;
  updatedAt: number;
};

/** A read-only pairing code, minted by the computer, for another device. */
export type ReadOnlyTicketOffer = {
  uri: string;
  /** Empty when the host drew no QR, or drew something unsafe to insert. */
  qrSvg: string;
  expiresAt: number;
  desktopName: string;
};

export type CoverReport = { reportId: string; queued: boolean };

/** The NIP-56 reasons a cover report may use. */
export const reportReasons = [
  { value: 'spam', label: 'Spam or advertising' },
  { value: 'illegal', label: 'Illegal content' },
  { value: 'malware', label: 'Malware or a scam' },
  { value: 'impersonation', label: 'Wrong artist or album' },
  { value: 'nudity', label: 'Nudity' },
  { value: 'profanity', label: 'Profanity' },
  { value: 'other', label: 'Something else' }
] as const;
export type ReportReason = (typeof reportReasons)[number]['value'];
