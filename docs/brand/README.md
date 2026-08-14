# The MediaLith mark

Two standing stones with the play triangle struck across both of them, cut in two by the
gap between them.

Nothing in it is decoration, and that is the test any future change to it has to pass:

| Part | What it is |
| --- | --- |
| Two stones | The image has two slots and only ever two. ADR-0003 froze that and it cannot be renumbered in the field |
| Unequal heights | Only one of them booted. The pair is symmetrical in function and never in state |
| Tops struck off at an angle | A rounded rectangle is a box. A slab whose top was broken is something quarried, which is the word in the name |
| The triangle survives the cut | Same media whichever slot is running. It is severed and still reads, which is what a rollback is |

## The files

| File | What it is |
| --- | --- |
| `medialith-mark.svg` | The mark. Primary, **40 px and up** |
| `medialith-mark-small.svg` | One stone, one triangle. **Below 40 px**, including the favicon |
| `medialith-wordmark.svg` | The name alone, outlined |
| `medialith-lockup.svg` | Mark and name together, horizontal |

All four are a single path each, one colour, `fill-rule="evenodd"`, no mask and no
gradient. They take `currentColor`, so there is no light file, no dark file and no accent
file — the mark is whatever colour the text around it is. That is also what makes the
knockouts true knockouts: whatever is behind the mark shows through the triangle.

## Where it is used

- **The console page** — `crates/plexosd/src/ui/console.html`, twice: inline in the header
  beside the product name, and as the tab icon in a `data:` URI. The page is one file with
  no external references and a test enforces that, so both are inlined rather than served.
  The favicon carries its own `prefers-color-scheme` rule, because browser chrome is not
  the page and `currentColor` means nothing there.
- **Anywhere else** — take the file, do not redraw it.

## Two surfaces it is deliberately not on

**The attached screen** (`plexosd::dashboard`) is a text console. A block-character version
of the mark could be drawn, and it is not, because there is no way to judge it from here:
the font is whatever the kernel compiled in, and this project's own trap list records that
the only instrument for that screen is the person sitting in front of it.

**The boot splash.** A UKI can carry a `.splash` PE section and `systemd-boot` will display
it, and `post-image.sh` already assembles sections with `objcopy`, so the mechanism is a
few lines away. It is not done because it would be an unverified change to the boot path,
and because it needs a BMP — the mark is vector, and rasterising it is a build dependency
this image does not have. If it is ever added, it is added with a machine to try it on.

## Rules

1. One colour. It inherits `currentColor` and needs no variants.
2. Clear space of half the mark's width on every side.
3. Swap to the small variant below 40 px. Do not scale the primary down and hope.
4. Never colour the two stones differently, and never fill the triangle. Both were tried:
   colouring the stones separately breaks the triangle into two unrelated notches and the
   whole idea goes with it.
5. Never set it on a busy photograph. The knockout is the mark, and it needs a ground to
   cut against.

## The wordmark

Noto Sans Bold, shaped with HarfBuzz using the font's own kerning, tracked -0.028em, and
converted to outlines. Noto Sans is **SIL Open Font Licence 1.1**, so it may be shipped and
modified.

Outlines rather than a `<text>` element on purpose. Set in a font stack it was a different
wordmark on every machine, and the one that got approved was whatever the viewer happened
to have installed — which is not a design decision, it is an accident that looked like one.

One thing worth knowing before anyone re-sets it: the design asked for *Media* at weight
650 and *Lith* at 800, and that distinction never rendered. Only Regular and Bold were
installed, so the browser drew both Bold. Uniform Bold is what was approved and what the
file contains.

## What was rejected

Three concepts were drawn, rendered at every size and looked at. None of the faults below
is visible in the code.

- **Verity** — a hash tree folding left to right into one root, tapering so the silhouette
  is also a play button. The best idea of the three and the worst drawing: three separated
  columns read as a bar chart, never as a triangle, and at 20 px it is noise.
- **Strata** — six layers for the six frozen partitions, one lit for the slot that booted.
  Robust at every size, and reads as a stack of pancakes or a list with a row selected.
- **A single slab with a seam** — the A/B split as a line across the upper third. The seam
  plus the angled top reads as a lid: at 96 px it is a tin.

---

MediaLith is an independent project. It is not affiliated with, endorsed by or sponsored by
Plex Inc. Plex and Plex Media Server are trademarks of Plex Inc.
