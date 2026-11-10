import re


def slugify(text):
    text = text.lower()
    return re.sub(r"[^a-z0-9]", "-", text)
