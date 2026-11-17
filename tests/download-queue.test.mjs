import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import ts from 'typescript';

const page = readFileSync(new URL('../src/routes/+page.svelte', import.meta.url), 'utf8')
  .split('<script lang="ts">')[1].split('</script>')[0];
const ast = ts.createSourceFile('page.ts', page, ts.ScriptTarget.Latest, true);
const names = new Set(['selectedResults', 'canDownloadResult', 'downloadSelectedResults',
  'isActiveTransfer', 'isCompleteTransfer', 'isLocalFile', 'mergePendingTransfers',
  'showAvailableDownloadSlots', 'applyTransferUpdate', 'refreshLocalLibrary', 'syncResultLocality',
  'requestNetworkDownload', 'startDownload',
  'removeTransfer', 'clearAllTransfers']);
const source = ast.statements.filter((node) => ts.isFunctionDeclaration(node) && names.has(node.name.text))
  .map((node) => node.getText(ast)).join('\n');
const tick = () => new Promise((resolve) => setImmediate(resolve));

function fixture({ failedFile, localFiles = [], existingFiles = [] } = {}) {
  const results = Array.from({ length: 5 }, (_, index) => ({
    fileId: `song-${index}`, name: `Song ${index}`, size: '1 MB', sourceDetails: [{ pubkey: 'source' }]
  }));
  const requests = new Map();
  let nextId = -1;
  const nativeRow = (fileId) => ({ id: nextId--, fileId, filename: fileId, size: 100,
    progress: 0, status: 'Queued', speed: 'Queued', destination: '' });
  let rows = existingFiles.map(nativeRow);
  const context = vm.createContext({
    results, selectedResultIds: new Set(results.map((item) => item.fileId)),
    nativeReady: true, downloadingSelection: false, clearingTransfers: false, paused: false,
    downloadLibraryRefreshNeeded: false, napstrFolder: '/old-music',
    downloadGeneration: 0, nextOptimisticTransferId: 1000,
    pendingDownloadRequests: new Map(), downloadAttempts: new Map(), cancellingFiles: new Set(),
    startingDownloads: new Set(), removingTransfers: new Set(), audiobookDownloads: [],
    transfers: [...rows], activityMessage: '', selected: null,
    localFileIds: new Set(localFiles), sharedFiles: [], indexedBytes: 0, localAudiobooks: [],
    currentTrack: null, selectedShared: null, selectedTagFile: null,
    downloadLibraryPage: 0, sharedLibraryPage: 0, resultPage: 0,
    resultUser: null, identityNpub: 'self', resultsAreNetwork: true,
    localPageCount: () => 1, resultPageCount: () => 1, visibleSharedFiles: () => [],
    readableSize: String, reconcileResultSelection: () => {},
    mapTransfers: (items) => items.map((item) => ({ ...item, name: item.filename })),
    msg: (key, values = {}) => key.replace(/\{(\w+)\}/g, (_, name) => values[name]),
    invoke: async (command, args) => {
      if (command === 'request_network_download') {
        await new Promise((resolve) => requests.set(args.fileId, resolve));
        if (args.fileId === failedFile) throw new Error('Seeder unavailable');
        rows.push(nativeRow(args.fileId));
        return args.fileId;
      }
      if (command === 'get_transfers') return rows.map((row) => ({ ...row }));
      if (command === 'get_snapshot') return { settings: { napstrFolder: '/music' }, files: localFiles.map((fileId) => ({ fileId, filename: fileId, size: 100 })), audiobooks: [], indexedBytes: 100 };
      if (command === 'remove_transfer') { rows = rows.filter((row) => row.id !== args.id); return; }
      if (command === 'clear_all_transfers') { rows = []; return; }
      throw new Error(command);
    }
  });
  vm.runInContext(ts.transpile(source, { target: ts.ScriptTarget.ES2022 }), context);
  return { context, requests, rows: () => rows, settleAll: () => requests.forEach((resolve) => resolve()) };
}

test('Download All submits every selected track before any request finishes', async () => {
  const f = fixture();
  const batch = f.context.downloadSelectedResults();
  assert.equal(f.requests.size, 5, 'a stalled first request must not hold up the selection');
  assert.equal(f.context.transfers.length, 5);
  assert.equal(new Set(f.context.transfers.map((row) => row.id)).size, 5, 'cancel buttons need distinct IDs');
  assert.equal(f.context.transfers.filter((row) => row.status === 'Starting download').length, 2);
  assert.equal(f.context.transfers.filter((row) => row.status === 'Queued').length, 3);

  // Complete one request out of order. Its refresh must retain all pending rows.
  f.requests.get('song-3')();
  await tick();
  assert.equal(f.context.transfers.length, 5);
  f.settleAll();
  await batch;
  assert.equal(f.rows().length, 5);
  assert.equal(f.context.transfers.length, 5);
  assert.match(f.context.activityMessage, /requested: 5 · Skipped: 0 · Failed: 0/);
});

test('one failed enqueue does not stop the rest of the selection', async () => {
  const f = fixture({ failedFile: 'song-0' });
  const batch = f.context.downloadSelectedResults();
  f.settleAll();
  await batch;
  assert.equal(f.rows().length, 4);
  assert.match(f.context.activityMessage, /requested: 4 · Skipped: 0 · Failed: 1/);
});

test('bulk downloads skip local and already queued tracks and reject duplicate clicks', async () => {
  const f = fixture({ localFiles: ['song-0'], existingFiles: ['song-1'] });
  const batch = f.context.downloadSelectedResults();
  await f.context.downloadSelectedResults();
  assert.deepEqual([...f.requests.keys()], ['song-2', 'song-3', 'song-4']);
  f.settleAll();
  await batch;
  assert.equal(f.rows().length, 4);
  assert.match(f.context.activityMessage, /requested: 3 · Skipped: 2 · Failed: 0/);
});

test('cancelling one pending track leaves the other selected tracks queued', async () => {
  const f = fixture();
  const batch = f.context.downloadSelectedResults();
  const target = f.context.transfers.find((row) => row.fileId === 'song-2');
  const cancel = f.context.removeTransfer(target.id);
  await tick();
  f.settleAll();
  await Promise.all([batch, cancel]);
  assert.equal(f.rows().length, 4);
  assert.ok(f.rows().every((row) => row.fileId !== 'song-2'));
  assert.equal(f.context.transfers.length, 4);
});

test('clear all drains every pending enqueue without restoring cancelled tracks', async () => {
  const f = fixture();
  const batch = f.context.downloadSelectedResults();
  const clear = f.context.clearAllTransfers();
  await tick();
  f.settleAll();
  await Promise.all([batch, clear]);
  assert.equal(f.rows().length, 0);
  assert.equal(f.context.transfers.length, 0);
  assert.equal(f.context.pendingDownloadRequests.size, 0);
});

test('free slots start immediately; only tracks behind two active downloads show Queued', () => {
  const f = fixture();
  const row = (id, status = 'Queued', progress = 0) => ({ id, fileId: `song-${id}`, status, progress });
  for (const active of [0, 1, 2]) {
    const downloading = Array.from({ length: active }, (_, index) => row(index, 'Downloading', 50));
    const shown = f.context.showAvailableDownloadSlots([row(4), row(3), row(2), ...downloading]);
    assert.equal(shown.filter((item) => item.status === 'Starting download').length, 2 - active);
    assert.equal(shown.filter((item) => item.status === 'Queued').length, 1 + active);
  }
  const atCapacity = f.context.showAvailableDownloadSlots([row(2), row(1, 'Downloading', 100), row(0, 'Downloading', 10)]);
  assert.equal(atCapacity[0].status, 'Queued', '100% still occupies a slot until verification completes');
  f.context.paused = true;
  assert.equal(f.context.showAvailableDownloadSlots([row(0)])[0].status, 'Queued');
});

test('100% keeps polling until verified completion and disappearance updates Tor to Local', async () => {
  const localFiles = [];
  const f = fixture({ localFiles });
  f.context.transfers = [{ id: -1, fileId: 'song-0', name: 'Song 0', progress: 100, status: 'Downloading' }];
  assert.equal(f.context.isActiveTransfer(f.context.transfers[0]), true);
  localFiles.push('song-0');
  // This is the same update path used by events, polling and request refreshes.
  await f.context.applyTransferUpdate([]);
  assert.equal(f.context.napstrFolder, '/music');
  assert.equal(f.context.results[0].speed, 'Local');
  assert.equal(f.context.results[0].remote, false);
  assert.equal(f.context.isLocalFile('song-0'), true);
  assert.equal(f.context.results[1].speed, 'Tor');
  assert.equal(f.context.canDownloadResult(f.context.results[0]), false);
});

test('failed library refresh is retried even after the last transfer has disappeared', async () => {
  const localFiles = ['song-0'];
  const f = fixture({ localFiles });
  f.context.localFileIds.clear();
  f.context.transfers = [{ id: -1, fileId: 'song-0', progress: 100, status: 'Downloading' }];
  const invoke = f.context.invoke;
  f.context.invoke = async () => { throw new Error('Temporary snapshot failure'); };
  await f.context.applyTransferUpdate([]);
  assert.equal(f.context.downloadLibraryRefreshNeeded, true);
  f.context.invoke = invoke;
  await f.context.applyTransferUpdate([]);
  assert.equal(f.context.downloadLibraryRefreshNeeded, false);
  assert.equal(f.context.napstrFolder, '/music');
  assert.equal(f.context.results[0].speed, 'Local');
});
