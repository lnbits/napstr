<script lang="ts">
  /**
   * Where a track actually is: in this phone's audio cache, on the paired
   * Napstr computer, or only out on the network among its seeders.
   *
   * `local` on the wire means the host holds the file, so it is the middle
   * state; the phone's own cache has to be asked about separately.
   */
  import type { RemoteTrack } from './types';

  let { track, cached = false, pending = false }: {
    track: RemoteTrack;
    cached?: boolean;
    pending?: boolean;
  } = $props();
</script>

{#if pending}
  <span class="track-badge pending" role="img" aria-label="Downloading from Napstr">
    <i></i>
  </span>
{:else if cached}
  <span class="track-badge phone" role="img" aria-label="Cached on this phone">
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <rect x="6.6" y="2.4" width="10.8" height="19.2" rx="2.8" />
      <path d="M12 7.4v5.4" /><path d="M9.8 10.9 12 13.1l2.2-2.2" />
    </svg>
  </span>
{:else if track.local}
  <span class="track-badge computer" role="img" aria-label="Stored on your Napstr computer">
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <rect x="2.8" y="4.2" width="18.4" height="12.4" rx="2.2" />
      <path d="M12 16.6V20.4" /><path d="M8.6 20.4h6.8" />
    </svg>
  </span>
{:else}
  <span
    class="track-badge network"
    role="img"
    aria-label={`${track.sources.length} ${track.sources.length === 1 ? 'seeder' : 'seeders'} on the network`}
  >
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <circle class="filled" cx="12" cy="12" r="2.3" />
      <path d="M6.6 17.4a7.6 7.6 0 0 1 0-10.8" /><path d="M17.4 6.6a7.6 7.6 0 0 1 0 10.8" />
    </svg>
    <b>{track.sources.length}</b>
  </span>
{/if}
