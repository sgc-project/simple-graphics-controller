# Cursor artwork

The PNGs here are the shapes the graphics controller draws. They are **not**
loaded at runtime: `build.rs` runs `tools/gen_cursors.py` at compile time, which
decodes, premultiplies and (if needed) rescales them into a Rust table in
`OUT_DIR`, which the daemon `include!`s. The shipped binary contains pixels, not
images, and no image decoder.

Any shape without a PNG here falls back to the drawn set in the generator, so
the table is always complete — a partial theme cannot ship a cursor set with
holes in it.

## Provenance

Supplied by the project owner and released as **public domain**. No attribution
is required; this note exists so the licence is recorded next to the files rather
than in a conversation nobody can find later.

## Shapes

| file | shape | hotspot | where the hotspot came from |
|---|---|---|---|
| `default.png` | `Default` | (3, 2) | the arrow's tip, read off the 24 px art |
| `pointer.png` | `Pointer` | (9, 0) | the hand's fingertip |
| `text.png` | `Text` | (12, 12) | the I-beam's middle |
| `grabbing.png` | `Grabbing` | (12, 12) | the fist's centre |

The hotspot is the pixel in the image that lands on the pointer position. A PNG
does not carry one, so it is declared in `HOTSPOTS` in `tools/gen_cursors.py`;
shapes that come from the drawn set carry theirs next to the drawing.

## Adding or replacing a shape

1. Drop a PNG named after the shape, in snake_case: `resize_top_left.png`,
   `wait.png`, `grab.png`. The list of names is the `SHAPES` table in the
   generator.
2. 8-bit RGB or RGBA, non-interlaced, any size — the build infers the cursor size
   from the largest icon, and the drawn fallbacks scale to match.
3. Declare the hotspot in `HOTSPOTS` if the shape is new.
4. `cargo build`: the art is regenerated, and `--preview` on the generator prints
   the shapes as ASCII if you want to check a hotspot before running anything on
   a board.

Preview without building:

    python3 tools/gen_cursors.py --icons cursor-icons --preview --out /tmp/art.rs
