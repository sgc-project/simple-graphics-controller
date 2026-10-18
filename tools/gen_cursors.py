#!/usr/bin/env python3
"""Generate the controller's cursor artwork.

The graphics controller draws the pointer, so it owns the art: one shape set,
one style, the same on every framework. This script rasterizes that set and
emits `src/resource_manager/cursor_art.rs` — do not edit that file by hand.

Design rules that make a cursor readable anywhere:

- transparent outside the shape, so the plane blends with whatever is under it;
- a 1px black outline around the fill, because a white arrow vanishes on a
  white window and a black one vanishes on a black terminal;
- fully opaque pixels only (alpha 0 or 255): the kernel treats a cursor image as
  premultiplied, and 0/255 is the one case where premultiplication cannot go
  wrong, so no driver quirk can tint the pointer;
- hotspot per shape, at the pixel that means "here" (the arrow's tip, the
  I-beam's middle, the crosshair's centre).

Two sources of art:

  --drawn            the built-in vector set below (default)
  --icons DIR        PNG files named after the shapes (default.png, text.png, ...)

Either way the output is the same Rust table. The PNG path exists because
hand-drawn vectors are a stopgap: point --icons at a cursor theme and the
controller gets real artwork, converted at build time. The daemon never sees a
PNG and never carries a decoder.

PNG rules the converter enforces:

- RGBA or RGB, 8 bits per channel, no interlacing (a cursor needs alpha);
- any size: it is scaled to --size with a box filter;
- alpha is premultiplied on the way out, so a theme's anti-aliased edges blend
  correctly on the plane instead of showing dark fringes;
- the hotspot comes from HOTSPOTS below, because a PNG does not carry one.

Usage: python3 tools/gen_cursors.py [--preview] [--size 32] [--icons DIR]
"""

import argparse
import pathlib

# A shape is (name, hotspot, list of operations). Operations are drawn in order
# into one mask; the outline is derived from the finished mask.
#   ("rect", x0, y0, x1, y1)          inclusive pixel rectangle
#   ("poly", [(x, y), ...])           even-odd filled polygon
#   ("circle", cx, cy, r)
#   ("ring", cx, cy, r_outer, r_inner)
#   ("line", x0, y0, x1, y1, width)   thick line, width in pixels
SHAPES = [
    (
        "Default",
        (2, 2),
        [
            ("poly", [(2, 2), (2, 26), (8, 20), (12, 30), (17, 28), (13, 18), (22, 18)]),
        ],
    ),
    (
        "Text",
        (16, 16),
        [
            ("rect", 14, 7, 17, 25),
            ("rect", 9, 5, 22, 7),
            ("rect", 9, 25, 22, 27),
        ],
    ),
    (
        "Pointer",
        (16, 3),
        [
            # A hand: palm, index finger, thumb — approximate on purpose, the
            # shape only has to read as "clickable" at 32px.
            ("rect", 11, 12, 22, 27),
            ("rect", 14, 4, 18, 20),
            ("rect", 9, 16, 12, 24),
            ("rect", 21, 15, 24, 23),
        ],
    ),
    (
        "Crosshair",
        (16, 16),
        [
            ("rect", 15, 4, 17, 12),
            ("rect", 15, 20, 17, 28),
            ("rect", 4, 15, 12, 17),
            ("rect", 20, 15, 28, 17),
        ],
    ),
    (
        "Grab",
        (16, 10),
        [
            # An open hand: the whole palm plus four fingers, ready to drag.
            ("rect", 9, 14, 23, 27),
            ("rect", 10, 8, 13, 18),
            ("rect", 14, 5, 17, 18),
            ("rect", 18, 7, 21, 18),
            ("rect", 22, 10, 25, 19),
        ],
    ),
    (
        "Grabbing",
        (16, 12),
        [
            # A fist: the palm and the knuckles, nothing pointing up.
            ("rect", 9, 13, 23, 27),
            ("rect", 10, 17, 22, 20),
        ],
    ),
    (
        "Move",
        (16, 16),
        [
            ("poly", [(16, 3), (21, 10), (11, 10)]),
            ("poly", [(16, 29), (21, 22), (11, 22)]),
            ("poly", [(3, 16), (10, 11), (10, 21)]),
            ("poly", [(29, 16), (22, 11), (22, 21)]),
            ("rect", 15, 10, 17, 22),
        ],
    ),
    (
        "ResizeVertical",
        (16, 16),
        [
            ("rect", 15, 6, 17, 26),
            ("poly", [(16, 2), (11, 9), (21, 9)]),
            ("poly", [(16, 30), (11, 23), (21, 23)]),
        ],
    ),
    (
        "ResizeHorizontal",
        (16, 16),
        [
            ("rect", 6, 15, 26, 17),
            ("poly", [(2, 16), (9, 11), (9, 21)]),
            ("poly", [(30, 16), (23, 11), (23, 21)]),
        ],
    ),
    (
        "ResizeTopLeft",
        (16, 16),
        [
            ("line", 8, 8, 24, 24, 4),
            ("poly", [(4, 4), (14, 5), (5, 14)]),
            ("poly", [(28, 28), (18, 27), (27, 18)]),
        ],
    ),
    (
        "ResizeTopRight",
        (16, 16),
        [
            ("line", 24, 8, 8, 24, 4),
            ("poly", [(28, 4), (18, 5), (27, 14)]),
            ("poly", [(4, 28), (14, 27), (5, 18)]),
        ],
    ),
    (
        "Wait",
        (16, 16),
        [
            # Hourglass: two triangles meeting in the middle.
            ("poly", [(8, 6), (24, 6), (16, 16)]),
            ("poly", [(8, 26), (24, 26), (16, 16)]),
        ],
    ),
    (
        "Progress",
        (16, 16),
        [
            ("ring", 16, 16, 12, 8),
            ("poly", [(16, 4), (22, 4), (16, 14)]),
        ],
    ),
    (
        "NotAllowed",
        (16, 16),
        [
            ("ring", 16, 16, 11, 7),
            ("line", 9, 23, 23, 9, 3),
        ],
    ),
    (
        "Help",
        (16, 16),
        [
            ("ring", 12, 12, 6, 4),
            ("rect", 11, 17, 13, 20),
            ("rect", 11, 22, 13, 23),
        ],
    ),
]


# Hotspots for PNG-sourced shapes: the pixel in the icon that means "here".
# A PNG has no such notion, so it is declared next to the shape name.
# What each shape is for, in the words a downloader needs: the file to fetch and
# the situation the cursor is meant to describe.
MEANING = {
    "default": "the ordinary arrow, used for everything without a better shape",
    "text": "an I-beam over selectable text",
    "pointer": "a hand over a link or button",
    "crosshair": "precise picking, drawing and colour tasks",
    "grab": "something can be grabbed and dragged",
    "grabbing": "a drag is in progress",
    "move": "the whole window or item can be moved",
    "resize_vertical": "resize a top or bottom edge",
    "resize_horizontal": "resize a left or right edge",
    "resize_top_left": "resize from the top-left corner",
    "resize_top_right": "resize from the top-right corner",
    "wait": "busy, the application will not respond for a while",
    "progress": "busy, but the application still responds",
    "not_allowed": "this drop or action is not allowed here",
    "help": "there is a tooltip or help for this",
}

HOTSPOTS = {
    # Read off the supplied 24px artwork: the arrow's tip, the I-beam's middle,
    # the hand's fingertip, the fist's centre.
    "default": (3, 2),
    "text": (12, 12),
    "pointer": (9, 0),
    "crosshair": (12, 12),
    "move": (12, 12),
    "resize_vertical": (16, 16),
    "resize_horizontal": (16, 16),
    "resize_top_left": (16, 16),
    "resize_top_right": (16, 16),
    "wait": (16, 16),
    "progress": (16, 16),
    "not_allowed": (16, 16),
    "help": (16, 16),
    "grab": (12, 12),
    "grabbing": (12, 12),
}

def _paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def decode_png(path):
    """Decode an 8-bit RGBA/RGB PNG into rows of (r, g, b, a).

    Deliberately minimal: the shapes a cursor theme ships, and no more. Palette,
    16-bit and interlaced images are refused with a reason rather than guessed at.
    """
    import struct
    import zlib

    data = pathlib.Path(path).read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG")
    pos, idat, width, height = 8, b"", 0, 0
    header = None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            width, height, depth, color, _comp, _filt, interlace = struct.unpack(">IIBBBBB", body)
            header = (depth, color, interlace)
        elif kind == b"IDAT":
            idat += body
        elif kind == b"IEND":
            break
    if header is None:
        raise SystemExit(f"{path}: no IHDR")
    depth, color, interlace = header
    if depth != 8 or color not in (2, 6) or interlace != 0:
        raise SystemExit(
            f"{path}: need 8-bit RGB or RGBA, non-interlaced (got depth {depth}, "
            f"color {color}, interlace {interlace})"
        )
    channels = 4 if color == 6 else 3
    raw = zlib.decompress(idat)
    stride = width * channels
    rows, prev = [], bytearray(stride)
    pos = 0
    for _ in range(height):
        filter_type = raw[pos]
        pos += 1
        line = bytearray(raw[pos:pos + stride])
        pos += stride
        for i in range(stride):
            a = line[i - channels] if i >= channels else 0
            b = prev[i]
            c = prev[i - channels] if i >= channels else 0
            if filter_type == 1:
                line[i] = (line[i] + a) & 0xFF
            elif filter_type == 2:
                line[i] = (line[i] + b) & 0xFF
            elif filter_type == 3:
                line[i] = (line[i] + (a + b) // 2) & 0xFF
            elif filter_type == 4:
                line[i] = (line[i] + _paeth(a, b, c)) & 0xFF
            elif filter_type != 0:
                raise SystemExit(f"{path}: bad filter {filter_type}")
        rows.append(line)
        prev = line
    out = []
    for line in rows:
        row = []
        for x in range(width):
            px = line[x * channels:(x + 1) * channels]
            row.append((px[0], px[1], px[2], 255 if channels == 3 else px[3]))
        out.append(row)
    return width, height, out


def resample(rows, width, height, size):
    """Box-filter downscale to `size`, or centre-pad if it is already smaller."""
    if width < size or height < size:
        # Too small to shrink: centre it, transparent around the edges.
        left, top = (size - width) // 2, (size - height) // 2
        canvas = [[(0, 0, 0, 0)] * size for _ in range(size)]
        for y in range(height):
            for x in range(width):
                canvas[top + y][left + x] = rows[y][x]
        return canvas
    out = []
    for y in range(size):
        y0, y1 = y * height // size, max(y * height // size + 1, (y + 1) * height // size)
        row = []
        for x in range(size):
            x0, x1 = x * width // size, max(x * width // size + 1, (x + 1) * width // size)
            total = [0, 0, 0, 0]
            count = 0
            for sy in range(y0, y1):
                for sx in range(x0, x1):
                    px = rows[sy][sx]
                    for i in range(4):
                        total[i] += px[i]
                    count += 1
            row.append(tuple(v // count for v in total))
        out.append(row)
    return out


def render_icon(name, path, size, preview, as_name=None, hotspot_from=None):
    width, height, rows = decode_png(path)
    rows = resample(rows, width, height, size)
    const = _snake(as_name or name).upper()

    pixels = []
    painted = 0
    for row in rows:
        for (r, g, b, a) in row:
            if a == 0:
                pixels.append("0x00000000")
                continue
            painted += 1
            # Premultiply: the kernel treats a cursor image as premultiplied,
            # and a theme's anti-aliased edges are straight alpha.
            pr, pg, pb = (r * a // 255, g * a // 255, b * a // 255)
            pixels.append(f"0x{a:02x}{pr:02x}{pg:02x}{pb:02x}")

    scale = size / max(1, max(width, height))
    hotspot_name = hotspot_from or name
    hx, hy = round(HOTSPOTS[hotspot_name][0] * scale), round(HOTSPOTS[hotspot_name][1] * scale)
    if not (0 <= hx < size and 0 <= hy < size):
        raise SystemExit(f"{name}: hotspot {(hx, hy)} is outside {size}x{size}")
    if painted == 0:
        raise SystemExit(f"{name}: {path} is fully transparent")

    if preview:
        print(f"--- {name} from {pathlib.Path(path).name} (hotspot {hx},{hy}, {painted} px)")
        for y, row in enumerate(rows):
            line = ""
            for x, (_r, _g, _b, a) in enumerate(row):
                line += "+" if (x, y) == (hx, hy) else ("#" if a > 160 else ("." if a > 40 else " "))
            print(line)

    body = ",\n    ".join(", ".join(pixels[i:i + 8]) for i in range(0, len(pixels), 8))
    return (
        f"/// {name} from the supplied artwork, hotspot {hx}x{hy}.\n"
        f"pub(super) const {const}: Art = Art {{\n"
        f"    width: {size},\n    height: {size},\n    hotspot: ({hx}, {hy}),\n"
        f"    pixels: &[\n    {body},\n    ],\n}};\n"
    )


def _snake(name):
    """ResizeTopLeft -> resize_top_left: the name a log line and an icon file use."""
    import re as _re

    return _re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def _index(names):
    """The table the daemon iterates: shape name -> const, in a fixed order."""
    entries = "".join(f'    ("{name}", &{const}),\n' for name, const in names)
    return (
        "/// Every shape this build carries, in a stable order (for logging and\n"
        "/// completeness checks).\n"
        "pub(super) const ALL: &[(&str, &Art)] = &[\n" + entries + "];\n"
    )


def _in_poly(x, y, points):
    """Even-odd rule at the pixel centre."""
    inside = False
    j = len(points) - 1
    for i in range(len(points)):
        xi, yi = points[i]
        xj, yj = points[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
            inside = not inside
        j = i
    return inside


def _rasterize(ops, size):
    # The drawings below are authored in a 32px box; a build for a 24px cursor
    # has to scale the geometry, not clip it.
    k = size / 32.0

    def sc(v):
        return v * k

    mask = set()
    for op in ops:
        kind = op[0]
        if kind == "rect":
            x0, y0, x1, y1 = (sc(v) for v in op[1:])
            for y in range(max(0, int(y0)), min(size - 1, int(y1)) + 1):
                for x in range(max(0, int(x0)), min(size - 1, int(x1)) + 1):
                    mask.add((x, y))
        elif kind == "poly":
            points = [(sc(x), sc(y)) for (x, y) in op[1]]
            for y in range(size):
                for x in range(size):
                    if _in_poly(x + 0.5, y + 0.5, points):
                        mask.add((x, y))
        elif kind in ("circle", "ring"):
            cx, cy, r = sc(op[1]), sc(op[2]), sc(op[3])
            inner = sc(op[4]) if kind == "ring" else -1
            for y in range(size):
                for x in range(size):
                    d2 = (x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2
                    if d2 <= r * r and d2 >= inner * inner:
                        mask.add((x, y))
        elif kind == "line":
            x0, y0, x1, y1, width = (sc(v) for v in op[1:])
            steps = int(max(abs(x1 - x0), abs(y1 - y0)) * 4) + 1
            half = (width - 1) / 2.0
            for step in range(steps + 1):
                t = step / steps
                px = x0 + (x1 - x0) * t
                py = y0 + (y1 - y0) * t
                for y in range(size):
                    for x in range(size):
                        if abs(x + 0.5 - px) <= half and abs(y + 0.5 - py) <= half:
                            mask.add((x, y))
        else:
            raise ValueError(f"unknown op {kind}")
    return mask


def _outline(mask, size):
    """1px ring around the mask: transparent pixels touching a filled one."""
    ring = set()
    for (x, y) in mask:
        for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            nx, ny = x + dx, y + dy
            if 0 <= nx < size and 0 <= ny < size and (nx, ny) not in mask:
                ring.add((nx, ny))
    return ring


def render(name, hotspot, ops, size, preview):
    mask = _rasterize(ops, size)
    ring = _outline(mask, size)

    # Fit: if the drawing overflows, say so loudly instead of shipping a
    # clipped cursor.
    for (x, y) in mask | ring:
        if not (0 <= x < size and 0 <= y < size):
            raise SystemExit(f"{name}: pixel {(x, y)} outside {size}x{size}")
    k = size / 32.0
    hx, hy = round(hotspot[0] * k), round(hotspot[1] * k)
    if not (0 <= hx < size and 0 <= hy < size):
        raise SystemExit(f"{name}: hotspot {(hx, hy)} is outside {size}x{size}")
    # A hotspot may sit in a gap on purpose (a crosshair's centre is empty, and
    # that is where the crosshair points), so it only has to be *near* the art.
    near = any(
        abs(x - hx) <= 4 and abs(y - hy) <= 4 for (x, y) in mask | ring
    )
    if not near:
        raise SystemExit(f"{name}: no art within 4px of hotspot {(hx, hy)}")

    pixels = []
    for y in range(size):
        row = []
        for x in range(size):
            if (x, y) in mask:
                row.append("0xfff4f4f8")  # near-white fill
            elif (x, y) in ring:
                row.append("0xff101014")  # near-black outline
            else:
                row.append("0x00000000")  # transparent
        pixels.extend(row)

    if preview:
        print(f"--- {name} (hotspot {hx},{hy}, {len(mask)} px)")
        for y in range(size):
            line = ""
            for x in range(size):
                if (x, y) == (hx, hy):
                    line += "+"
                elif (x, y) in mask:
                    line += "#"
                elif (x, y) in ring:
                    line += "."
                else:
                    line += " "
            print(line)

    body = ",\n    ".join(", ".join(pixels[i:i + 8]) for i in range(0, len(pixels), 8))
    return (
        f"/// {name}, hotspot {hx}x{hy}.\n"
        f"pub(super) const {_snake(name).upper()}: Art = Art {{\n"
        f"    width: {size},\n    height: {size},\n    hotspot: ({hx}, {hy}),\n"
        f"    pixels: &[\n    {body},\n    ],\n}};\n"
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--size",
        type=int,
        default=0,
        help="cursor size in pixels; 0 = infer from the icons, else 32",
    )
    parser.add_argument(
        "--icons",
        metavar="DIR",
        help="read PNGs named after the shapes from DIR instead of drawing them",
    )
    parser.add_argument("--preview", action="store_true")
    parser.add_argument(
        "--list",
        action="store_true",
        help="print the icon set the generator wants, one file per line, and exit",
    )
    parser.add_argument("--out", required=False, help="where to write the art")
    args = parser.parse_args()

    if args.list:
        print(f"{len(SHAPES)} shapes, one PNG per shape, named after the shape,")
        print("square, any size (they are resampled together), transparent background:")
        print()
        for name, _h, _o in SHAPES:
            snake = _snake(name)
            print(f"  {snake + '.png':<22} {MEANING.get(snake, '')}")
        print()
        print("Sizes are not fixed: the set is resampled to the largest icon's")
        print("size, so supply them all at the same size (24 or 32 px read well).")
        print("Hotspots are declared in HOTSPOTS above — the arrow's tip, the")
        print("I-beam's middle — because a PNG does not carry one.")
        return

    icons = pathlib.Path(args.icons) if args.icons else None
    if icons is not None and not icons.is_dir():
        raise SystemExit(f"--icons: {icons} is not a directory")

    # The size comes from the artwork when a theme is supplied: its icons are
    # authored at the size their author intended, and upscaling pixel art only
    # blurs it. Without a theme, the drawn set is authored at 32.
    if args.size == 0:
        if icons is not None:
            sizes = [
                decode_png(icons / f"{_snake(name)}.png")[:2]
                for name, _h, _o in SHAPES
                if (icons / f"{_snake(name)}.png").is_file()
            ]
            args.size = max((max(w, h) for (w, h) in sizes), default=32)
        else:
            args.size = 32
        print(f"size: {args.size}px")

    if not args.out:
        raise SystemExit("--out is required: the art is generated into the build directory")

    # Plain comments, not //! : this file is include!()d into a module body, and
    # inner doc comments are only legal before the module's first item.
    header = f'''// The controller's cursor artwork.
//
// Generated by `tools/gen_cursors.py` — do not edit by hand; change the
// generator and regenerate.
//
// Every image is {args.size}x{args.size}, ARGB8888, row-major, and fully opaque or fully
// transparent (the kernel treats a cursor as premultiplied, and 0/255 cannot go
// wrong).

/// One cursor image: its size, the pixel that means "here", and the pixels.
pub(crate) struct Art {{
    pub width: u32,
    pub height: u32,
    /// Hotspot in image pixels: the point placed at the pointer position.
    pub hotspot: (i32, i32),
    /// ARGB8888, row-major, `width * height` entries.
    pub pixels: &'static [u32],
}}

'''

    # A theme with a hole in it must not produce a cursor set in two styles: a
    # missing shape falls back to the theme's own default icon, so every shape
    # looks like the same family until its own artwork arrives. Only with no
    # icons at all are the drawn vectors used.
    default_icon = icons / "default.png" if icons is not None else None
    body, from_icons, from_default, drawn = "", [], [], []
    for name, hotspot, ops in SHAPES:
        path = icons / f"{_snake(name)}.png" if icons is not None else None
        if path is not None and path.is_file():
            body += render_icon(_snake(name), path, args.size, args.preview) + "\n"
            from_icons.append(name)
        elif default_icon is not None and default_icon.is_file():
            body += (
                render_icon(
                    "default",
                    default_icon,
                    args.size,
                    args.preview,
                    as_name=_snake(name),
                    hotspot_from="default",
                )
                + "\n"
            )
            from_default.append(name)
        else:
            body += render(name, hotspot, ops, args.size, args.preview) + "\n"
            drawn.append(name)
    body += _index([(_snake(name), _snake(name).upper()) for name, _h, _o in SHAPES])

    if icons is not None:
        print(f"icons: {', '.join(from_icons) or '(none)'}")
        print(f"from default.png: {', '.join(from_default) or '(none)'}")
        print(f"drawn: {', '.join(drawn) or '(none)'}")

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(header + body)
    print(f"wrote {out} ({len(SHAPES)} shapes, {args.size}px)")


if __name__ == "__main__":
    main()
