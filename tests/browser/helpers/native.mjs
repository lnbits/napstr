export async function mockNative(page, { app = 'napstrfy', nativeLocale = 'en-GB', saved, paired = true, blockedStorage = false, platform = 'linux', remote = false } = {}) {
  await page.route('https://**', (route) => route.fulfill({ contentType: 'application/json', body: '{"results":[]}' }));
  await page.addInitScript(({ app, nativeLocale, saved, paired, blockedStorage, platform, remote }) => {
    if (saved) localStorage.setItem('napstr-language', saved);
    if (blockedStorage) {
      Storage.prototype.getItem = () => { throw new DOMException('Disabled', 'SecurityError'); };
      Storage.prototype.setItem = () => { throw new DOMException('Disabled', 'SecurityError'); };
    }
    window.calls = [];
    window.nativeLocale = nativeLocale;
    const track = { fileId: 'a'.repeat(64), filename: 'Search.wav', title: 'Search', artist: 'Settings', album: 'User album', folder: '', path: '/music/Search.wav', format: 'WAV', mime: 'audio/wav', size: 1234567, tags: 'Rock', local: !remote, sources: [], license: '', description: '', status: 'Shared' };
    const episode = { id: 'episode', title: 'Original episode', feedTitle: 'Original podcast', audioUrl: 'https://example.com/episode.mp3', datePublished: 1700000000, duration: 100, description: '', image: '', mime: 'audio/mpeg' };
    const status = () => ({ paired, connected: paired, desktopName: 'Music computer', streamOnly: false, endpointId: 'endpoint', libraryRevision: 1, error: '' });
    const transfers = [{ id: 1, fileId: track.fileId, filename: track.filename, size: track.size, progress: 100, status: 'Verified · Complete', speed: '', destination: '/music/Search.wav' }, { id: 2, fileId: 'b'.repeat(64), filename: 'Downloading.wav', size: 100, progress: 12, status: 'Downloading', speed: '', destination: '' }];
    window.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
      transformCallback: () => 1,
      unregisterCallback: () => {},
      invoke: async (cmd, args = {}) => {
        window.calls.push({ cmd, args });
        switch (cmd) {
          case 'plugin:os|locale': return window.nativeLocale;
          case 'plugin:app|version': return '0.2.2';
          case 'plugin:event|listen': return 1;
          case 'plugin:event|unlisten': return;
          case 'get_snapshot':
          case 'save_settings': return { native: true, indexedBytes: track.size, files: [track], audiobooks: [], transfers, settings: { napstrFolder: '/music', nostrRelays: '', displayName: 'Original user', profileAbout: 'Original profile', profilePicture: '' } };
          case 'get_transfers': return transfers;
          case 'start_network':
          case 'network_status': return { connected: true, pubkey: 'c'.repeat(64), npub: 'npub1test', relayCount: 1, torRunning: true, torStarting: false, torProgress: 100, torError: '', error: '' };
          case 'network_browse': return { results: [track], cursor: null, totalAvailable: 1 };
          case 'network_search':
          case 'search_catalog': return [track];
          case 'network_search_audiobooks': return [];
          case 'get_track_discussion_messages':
          case 'get_trollbox_messages': return [{ eventId: 'event', content: 'Search', displayName: 'Settings', npub: 'npubother', pubkey: 'd'.repeat(64), createdAt: 1700000000 }];
          case 'mobile_status': return { running: true, online: true, endpointId: 'endpoint', error: '', devices: [] };
          case 'play_audio': return { fileId: track.fileId, currentTime: 0, duration: 60, playing: true, ended: false, error: '' };
          case 'client_platform': return platform;
          case 'companion_status': return status();
          case 'cached_library': return { ...status(), tracks: paired ? [track] : [], total: paired ? 1 : 0 };
          case 'remote_library': return { tracks: [track], total: 1 };
          case 'remote_search': if (window.searchError) throw window.searchError; return [track];
          case 'remote_transfers': return [{ ...transfers[1], fileId: track.fileId }];
          case 'reconcile_audio_cache': return true;
          // The native side draws the track code, so answer the way it does.
          case 'track_code': return '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4"><rect width="4" height="4" fill="#ffffff"/><path fill="#000000" d="M0 0h1v1H0z"/></svg>';
          case 'cache_remote_audio': return { url: location.origin + (window.nextMediaSource || '/fixture.wav'), track: args.track };
          case 'podcast_playback_url': return { url: location.origin + '/fixture.wav', downloaded: false };
          case 'prefetch_remote_audio': return;
          case 'remote_audiobook_library': return { audiobooks: [], total: 0 };
          case 'podcast_downloads': return [{ episode, ready: false, status: 'Downloading', progress: 20 }];
          case 'podcast_parse_search': return [{ id: 1, title: 'Original podcast', author: 'Original author', feedUrl: 'https://example.com/feed', image: '', description: '', language: 'en', episodeCount: 1, genres: ['Music'] }];
          case 'podcast_episodes': return [episode];
          case 'pair_desktop': paired = true; return 'Music computer';
          case 'forget_desktop': paired = false; return;
          default: return null;
        }
      }
    };
  }, { app, nativeLocale, saved, paired, blockedStorage, platform, remote });
}

export function silentAudio() {
  const wav = Buffer.alloc(44 + 8000 * 2 * 60);
  wav.write('RIFF'); wav.writeUInt32LE(wav.length - 8, 4); wav.write('WAVEfmt ', 8);
  wav.writeUInt32LE(16, 16); wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22);
  wav.writeUInt32LE(8000, 24); wav.writeUInt32LE(16000, 28); wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34);
  wav.write('data', 36); wav.writeUInt32LE(wav.length - 44, 40);
  return wav;
}

export async function serveAudio(route) {
  const wav = silentAudio();
  const range = route.request().headers().range?.match(/^bytes=(\d+)-(\d*)$/);
  const headers = { 'Accept-Ranges': 'bytes' };
  if (range) {
    const start = Number(range[1]);
    const end = range[2] ? Math.min(Number(range[2]), wav.length - 1) : wav.length - 1;
    headers['Content-Range'] = `bytes ${start}-${end}/${wav.length}`;
    await route.fulfill({ status: 206, contentType: 'audio/wav', headers, body: wav.subarray(start, end + 1) });
  } else await route.fulfill({ contentType: 'audio/wav', headers, body: wav });
}
