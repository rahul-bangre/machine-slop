`slugify.py` turns a title into a URL slug, but its output is wrong for many
inputs.

Fix `slugify(text)` in `slugify.py` so that:

- the result is lowercase
- accented Latin letters lose their accents, so `é` becomes `e`
- every run of characters other than `a`-`z` and `0`-`9` becomes a single hyphen
- the result never starts or ends with a hyphen
- input that would produce an empty slug returns `untitled`

Keep the file name and the function name.
