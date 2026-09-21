<script lang="ts">
  import { artUrl, coverFor, type AlbumCover } from '$lib/artwork';

  /**
   * One album's artwork, resolved by the host.
   *
   * Every instance asks in the same tick, which `$lib/artwork` turns into one
   * batched call for the whole page. A missing or unreachable image is a blank
   * tile with a music note, never an error: the NIP is explicit that image URLs
   * rot and that a broken one is no reason to penalise anybody.
   *
   * `revision` is bumped by the page after a cover scan, which is what makes a
   * tile that answered "nothing known" ask again.
   */
  export let artist = '';
  export let album = '';
  export let preferThumb = false;
  export let size: 'small' | 'medium' | 'large' = 'small';
  export let revision = 0;

  let cover: AlbumCover | null = null;
  let failed = false;
  let generation = 0;

  $: identity = `${artist}\u0000${album}\u0000${revision}`;
  $: {
    const current = ++generation;
    cover = null;
    failed = false;
    // Reading `identity` here is what subscribes this block to it.
    if (identity) {
      void coverFor(artist, album).then((found) => {
        if (current === generation) cover = found;
      });
    }
  }
  $: url = failed ? '' : artUrl(cover, preferThumb);
  // Art somebody signed and published reads differently from art only this
  // computer resolved, and the tooltip is the cheapest honest way to say so.
  $: origin = cover?.author
    ? `Cover ${cover.source ? `(${cover.source}) ` : ''}published to Napstr`
    : 'Art resolved on this computer; not published';
</script>

{#if url}
  <span class="cover-art {size}" title={origin}>
    <img
      src={url}
      alt=""
      loading="lazy"
      decoding="async"
      onerror={() => (failed = true)}
    />
  </span>
{:else}
  <span class="cover-art {size} empty" aria-hidden="true">♪</span>
{/if}
