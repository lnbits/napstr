<script lang="ts">
  import { artworkFor, artworkHue } from './artwork';
  import type { RemoteTrack } from './types';

  let { track, lookup = false, large = false }: { track: RemoteTrack; lookup?: boolean; large?: boolean } = $props();
  let image = $state('');
  let failed = $state(false);
  let hue = $derived(artworkHue(track.fileId));

  $effect(() => {
    let alive = true;
    image = '';
    failed = false;
    if (lookup) artworkFor(track).then((url) => { if (alive) image = url; });
    return () => { alive = false; };
  });
</script>

<div class:large class="artwork" style={`--cover-hue:${hue}`}>
  {#if image && !failed}<img src={image} alt="" onerror={() => (failed = true)} />{:else}<img class="fallback" src="/napstr-logo-small.png" alt="" />{/if}
</div>
