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
  paired: boolean;
  connected: boolean;
  desktopName: string;
  endpointId: string;
  libraryRevision: number;
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
