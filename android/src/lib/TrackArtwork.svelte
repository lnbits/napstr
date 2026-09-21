<script lang="ts">
  import { ARTWORK_RETRY_MS, artworkHue, coverFor, coverRevision, dropCover } from './artwork';
  import type { RemoteTrack } from './types';

  /**
   * Every tile draws the small rendition. The full cover is the publisher's own
   * upload, so the views that show it big - the album header, the drawer - draw
   * it themselves, over a thumbnail rather than instead of one.
   *
   * `large` is the player bar's tile: the biggest one here, at the height of the
   * bar, and it loads at once instead of when it is scrolled into view.
   */
  let { track, lookup = true, large = false, onartworkchange }: { track: RemoteTrack; lookup?: boolean; large?: boolean; onartworkchange?: (url: string) => void } = $props();
  let image = $state('');
  let failed = $state(false);
  let visible = $state(false);
  let retry = $state(0);
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let hue = $derived(artworkHue(track.fileId));

  function observe(node: HTMLElement) {
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        visible = true;
        observer.disconnect();
      }
    }, { rootMargin: '150px' });
    observer.observe(node);
    return { destroy: () => observer.disconnect() };
  }

  function retryLater() {
    clearTimeout(retryTimer);
    if (retry < 2) retryTimer = setTimeout(() => { retry += 1; }, ARTWORK_RETRY_MS);
  }

  function imageFailed() {
    dropCover(track);
    failed = true;
    retryLater();
  }

  $effect(() => { track; retry = 0; });
  $effect(() => { onartworkchange?.(image && !failed ? image : ''); });

  $effect(() => {
    // Reading the revision subscribes this tile to it: when the host says its
    // art has changed, an album it had nothing for a moment ago is asked about
    // again, so art appears here as the desktop finds it.
    void $coverRevision;
    let alive = true;
    retry;
    image = '';
    failed = false;
    if (lookup && (large || visible)) {
      void coverFor(track).then((cover) => {
        if (!alive) return;
        if (!cover) {
          image = '';
          retryLater();
          return;
        }
        // The small rendition is what a tile wants, dense or not: the largest
        // one here is the player bar's, at the height of the bar itself. The
        // views that show a cover big - the album header, the drawer - draw the
        // full one themselves, over the small one they start from.
        image = cover.thumb || cover.art;
      });
    }
    return () => { alive = false; clearTimeout(retryTimer); };
  });
</script>

<div use:observe class:large class="artwork" style={`--cover-hue:${hue}`}>
  {#if image && !failed}<img src={image} alt="" onerror={imageFailed} />{:else}<img class="fallback" src="/napstr-logo-small.png" alt="" />{/if}
</div>
