#!/usr/bin/env python3
"""Check WCAG 2.x contrast ratios for chart colors.

The script compares each foreground color with one background color and
applies the threshold for the way the color is used:

* graphics (marks, lines, borders): 3.0:1
* text (labels, values, annotations): 4.5:1

Examples:

    python3 check_contrast.py --background "#ffffff" "#1f77b4" "#d62728"
    python3 check_contrast.py -b "#111111" -u text "#e6e6e6" "#9ecae1"
    python3 check_contrast.py --self-test

The exit status is 1 when at least one color fails the selected threshold,
and 0 otherwise. Usage errors exit with status 2.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import re
import sys

HEX_PATTERN = re.compile(r"^#?(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$")

THRESHOLDS = {"graphics": 3.0, "text": 4.5}


def parse_hex(value: str) -> tuple[int, int, int]:
    """Parse ``#rgb`` or ``#rrggbb`` (the ``#`` is optional)."""
    if not HEX_PATTERN.match(value):
        raise ValueError(f"not a hex color: {value!r} (expected #rgb or #rrggbb)")
    digits = value.lstrip("#")
    if len(digits) == 3:
        digits = "".join(character * 2 for character in digits)
    return tuple(int(digits[index : index + 2], 16) for index in (0, 2, 4))


def _linearize(channel: int) -> float:
    value = channel / 255.0
    if value <= 0.04045:
        return value / 12.92
    return ((value + 0.055) / 1.055) ** 2.4


def relative_luminance(rgb: tuple[int, int, int]) -> float:
    """WCAG relative luminance of an sRGB color."""
    red, green, blue = (_linearize(channel) for channel in rgb)
    return 0.2126 * red + 0.7152 * green + 0.0722 * blue


def contrast_ratio(first: tuple[int, int, int], second: tuple[int, int, int]) -> float:
    """Contrast ratio between two colors; the order does not matter."""
    a = relative_luminance(first)
    b = relative_luminance(second)
    lighter, darker = max(a, b), min(a, b)
    return (lighter + 0.05) / (darker + 0.05)


def evaluate(
    colors: list[str], background: str, usage: str
) -> list[tuple[str, float, bool]]:
    """Return ``(color, ratio, passed)`` for every foreground color."""
    minimum = THRESHOLDS[usage]
    background_rgb = parse_hex(background)
    results = []
    for value in colors:
        ratio = contrast_ratio(parse_hex(value), background_rgb)
        results.append((value, ratio, ratio >= minimum))
    return results


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Print WCAG 2.x contrast ratios between hex colors and a background, "
            "then flag ratios below 3:1 for graphics or 4.5:1 for text."
        ),
        epilog=(
            "Exit status: 0 when every color passes, 1 when any color fails, "
            "2 for invalid arguments."
        ),
    )
    parser.add_argument(
        "colors",
        nargs="*",
        metavar="COLOR",
        help="foreground colors as #rgb or #rrggbb",
    )
    parser.add_argument(
        "-b",
        "--background",
        default="#ffffff",
        metavar="COLOR",
        help="background color (default: #ffffff)",
    )
    parser.add_argument(
        "-u",
        "--usage",
        choices=tuple(THRESHOLDS),
        default="graphics",
        help="minimum contrast for the color's role (default: graphics = 3:1)",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the built-in checks and exit",
    )
    return parser


def run(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    if not args.colors:
        parser.error("provide at least one foreground color, or use --self-test")

    minimum = THRESHOLDS[args.usage]
    try:
        results = evaluate(args.colors, args.background, args.usage)
    except ValueError as error:
        parser.error(str(error))

    print(f"background {args.background}  usage {args.usage}  threshold {minimum:.1f}:1")
    failures = 0
    for value, ratio, passed in results:
        status = "PASS" if passed else "FAIL"
        print(f"{value:<9} {ratio:5.2f}:1  {status}")
        if not passed:
            failures += 1

    passed_count = len(results) - failures
    print(f"summary: {passed_count} passed, {failures} failed")
    return 1 if failures else 0


def self_test() -> int:
    """Exercise parsing, luminance, ratios, thresholds and exit codes."""
    checks: list[tuple[str, bool]] = []

    def expect(name: str, condition: bool) -> None:
        checks.append((name, bool(condition)))

    expect("black on white is 21:1", abs(contrast_ratio(parse_hex("#000000"), parse_hex("#ffffff")) - 21.0) < 1e-9)
    expect("contrast is symmetric", abs(contrast_ratio(parse_hex("#123456"), parse_hex("#abcdef")) - contrast_ratio(parse_hex("#abcdef"), parse_hex("#123456"))) < 1e-12)
    expect("short hex expands", parse_hex("#0f0") == (0, 255, 0))
    expect("hash is optional", parse_hex("a1b2c3") == (161, 178, 195))
    expect("relative luminance is ordered", relative_luminance(parse_hex("#000000")) < relative_luminance(parse_hex("#808080")) < relative_luminance(parse_hex("#ffffff")))
    expect("mid gray on white is below 4.5", contrast_ratio(parse_hex("#777777"), parse_hex("#ffffff")) < 4.5)
    expect("dark gray on white passes text", contrast_ratio(parse_hex("#555555"), parse_hex("#ffffff")) >= 4.5)
    expect("light gray fails graphics", contrast_ratio(parse_hex("#f0f0f0"), parse_hex("#ffffff")) < 3.0)
    expect("mid gray passes graphics", contrast_ratio(parse_hex("#949494"), parse_hex("#ffffff")) >= 3.0)
    expect("graphics threshold is 3:1", THRESHOLDS["graphics"] == 3.0)
    expect("text threshold is 4.5:1", THRESHOLDS["text"] == 4.5)

    invalid = [value for value in ("#12", "#12345", "red", "#gggggg") if _parses(value)]
    expect("invalid hex is rejected", not invalid)

    with contextlib.redirect_stdout(io.StringIO()):
        expect("passing colors exit 0", run(["--background", "#ffffff", "#000000", "#555555"]) == 0)
        expect("failing graphics exit 1", run(["--background", "#ffffff", "#f0f0f0"]) == 1)
        expect("failing text exits 1", run(["--background", "#ffffff", "--usage", "text", "#949494"]) == 1)

    failures = [name for name, passed in checks if not passed]
    for name, passed in checks:
        print(f"{'ok' if passed else 'FAILED'}: {name}")
    print(f"self-test: {len(checks) - len(failures)} passed, {len(failures)} failed")
    return 1 if failures else 0


def _parses(value: str) -> bool:
    try:
        parse_hex(value)
    except ValueError:
        return False
    return True


def main() -> None:
    sys.exit(run())


if __name__ == "__main__":
    main()
