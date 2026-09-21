<script lang="ts">
  export let forward = false;
</script>

<!--
  The notification's own 15-second glyphs, so the sheet and the system controls
  draw the same thing. The ring and its head are two paths because the head is
  turned 105 degrees clockwise about the point where it joins the ring; only
  those two mirror for the forward button. The numeral is stroked vector rather
  than text, so it cannot depend on a font being installed or on a baseline
  lining up, raised 2.5 units to sit centred in the ring and nudged 1.5 left in
  the mirrored icon, where the ring's centre moves.
-->
<svg viewBox="0 0 32 32" width="30" height="30" aria-hidden="true" focusable="false">
  <g transform={forward ? 'translate(32 0) scale(-1 1)' : undefined}>
    <path d="M9 9a11 11 0 1 1-3 8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />
    <g transform="rotate(105 9 9)">
      <path d="M9 3v6H3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />
    </g>
  </g>
  <path d="M11 16l2-1v8M17 15h5M17 15v4h3a2 2 0 0 1 0 4h-3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" transform={forward ? 'translate(-1.5 -2.5)' : 'translate(0 -2.5)'} />
</svg>
