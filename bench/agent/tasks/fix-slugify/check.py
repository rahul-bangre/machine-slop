import sys

from slugify import slugify

cases = {
    "Hello World": "hello-world",
    "  Leading and trailing  ": "leading-and-trailing",
    "Crème Brûlée!": "creme-brulee",
    "one -- two___three": "one-two-three",
    "Ünïcödé 2026": "unicode-2026",
    "already-a-slug": "already-a-slug",
    "!!!": "untitled",
    "": "untitled",
    "C++ & Rust": "c-rust",
}
wrong = {
    text: slugify(text) for text, slug in cases.items() if slugify(text) != slug
}
if wrong:
    for text, got in wrong.items():
        print(f"slugify({text!r}) returned {got!r}, expected {cases[text]!r}")
    sys.exit(1)
