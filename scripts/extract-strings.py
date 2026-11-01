"""Extract every translatable string of the app and the installer into gettext templates.

One catalog per crate holds both halves (LANGUAGE-PLAN.md): the `.slint` strings marked
`@tr(...)` and the Rust strings marked `tr!(...)` / `trn!(...)` / `tr_noop!(...)`. This writes them, with
plurals and the files they come from, to

    lightify-shell/translations/lightify-shell.pot
    lightify-installer/translations/lightify-installer.pot

which is what a translator starts a `<lang>/LC_MESSAGES/<domain>.po` from. Placeholders
(`{}`, `{0}`, `{n}`) are part of the English text and must stay as written. A comment
block right above a call that has a line starting `Translators:` becomes the entry's `#.`
note (from that line to the end of the block).

    python scripts/extract-strings.py                    write both templates
    python scripts/extract-strings.py lightify-shell     write just that one
    python scripts/extract-strings.py --check            are they up to date? (lists the
                                                         strings a regenerate adds/removes)
    python scripts/extract-strings.py --pseudo DOMAIN OUT.po

`--pseudo` writes the test language instead (every string ~40 % longer, accented and
bracketed). Only `lightify_i18n::build::emit` calls it, when a build sets LIGHTIFY_PSEUDO;
it is written into the build's own OUT_DIR and never ships.
"""
import math
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DOMAINS = {
    "lightify-shell": os.path.join(ROOT, "lightify-shell"),
    "lightify-installer": os.path.join(ROOT, "lightify-installer"),
}
SKIP_DIRS = {"target", "vendor", "translations", "node_modules", "__pycache__"}


def sources(crate, ext):
    for sub in ("ui", "src"):
        top = os.path.join(crate, sub)
        for dirpath, dirs, files in os.walk(top):
            dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS)
            for f in sorted(files):
                if f.endswith(ext):
                    yield os.path.join(dirpath, f)


# ── Lexing ────────────────────────────────────────────────────────────────────


def read_string(text, i, rust, translated=False):
    """The string literal starting at text[i] (a quote, or a Rust raw-string `r`).
    Returns (value, index after it)."""
    if rust and text[i] == "r":
        j = i + 1
        while text[j] == "#":
            j += 1
        hashes = j - i - 1
        end = text.index('"' + "#" * hashes, j + 1)
        return text[j + 1:end], end + 1 + hashes
    assert text[i] == '"', text[i:i + 20]
    out = []
    i += 1
    while True:
        c = text[i]
        if c == '"':
            return "".join(out), i + 1
        if c != "\\":
            out.append(c)
            i += 1
            continue
        e = text[i + 1]
        i += 2
        if e == "u":
            end = text.index("}", i)
            out.append(chr(int(text[i + 1:end], 16)))
            i = end + 1
        elif e == "x" and rust:
            out.append(chr(int(text[i:i + 2], 16)))
            i += 2
        elif e == "\n" and rust:
            while text[i] in " \t\r\n":
                i += 1
        elif e == "\r" and rust:
            i += 1
            while text[i] in " \t\r\n":
                i += 1
        elif e == "{" and not rust:
            # Slint interpolation, `\{expr}`: skip the expression (it may hold strings).
            if translated:
                raise ValueError(f"string interpolation inside @tr: {text[i - 2:i + 30]!r}")
            depth = 1
            while depth:
                if text[i] == '"':
                    _, i = read_string(text, i, rust)
                    continue
                depth += {"{": 1, "}": -1}.get(text[i], 0)
                i += 1
            out.append("{}")
        else:
            out.append({"n": "\n", "t": "\t", "r": "\r", "0": "\0"}.get(e, e))


def skip_space(text, i):
    """Past whitespace and comments (`//`, nested `/* */`)."""
    while i < len(text):
        if text[i].isspace():
            i += 1
        elif text.startswith("//", i):
            i = text.find("\n", i)
            i = len(text) if i < 0 else i
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        else:
            break
    return i


def calls(path, rust):
    """Every translation call in one file: (line, context, msgid, plural or None)."""
    text = open(path, encoding="utf-8").read()
    marker = re.compile(r"(?<![A-Za-z0-9_])(trn?!|tr_noop!)\s*\(" if rust else r"(@tr)\s*\(")
    found = []
    i = 0
    while i < len(text):
        c = text[i]
        if text.startswith("//", i) or text.startswith("/*", i):
            i = skip_space(text, i)
            continue
        raw = rust and c == "r" and re.match(r'r#*"', text[i:i + 8]) and not re.match(r"\w", text[i - 1:i])
        if c == '"' or raw:
            _, i = read_string(text, i, rust)
            continue
        if rust and c == "'":
            # A char literal ('"', '\'', '\u{..}') — or a lifetime, which has no closing quote.
            m = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'])'", text[i:i + 12])
            i += len(m.group(0)) if m else 1
            continue
        m = marker.match(text, i)
        if not m:
            i += 1
            continue
        line = text.count("\n", 0, i) + 1
        plural_macro = m.group(1) == "trn!"
        j = skip_space(text, m.end())
        first, j = read_string(text, j, rust, True)
        ctx, plural = "", None
        j = skip_space(text, j)
        if not rust and text.startswith("=>", j):
            ctx = first
            first, j = read_string(text, skip_space(text, j + 2), rust, True)
            j = skip_space(text, j)
        if not rust and text.startswith("|", j):
            plural, j = read_string(text, skip_space(text, j + 1), rust, True)
        if plural_macro:
            assert text[j] == ",", f"{path}:{line}: trn! needs two literals"
            plural, j = read_string(text, skip_space(text, j + 1), rust)
        found.append((line, ctx, first, plural, translator_note(text, m.start())))
        i = j
    return found


def translator_note(text, at):
    """A `// Translators: …` (or `/// Translators: …`) comment in the comment block right
    above the line at `at`, to the end of that block — written to the template as `#.`."""
    lines = text[:text.rfind("\n", 0, at) + 1].splitlines()
    block = []
    for raw in reversed(lines):
        s = raw.strip()
        if not s.startswith("//"):
            break
        block.insert(0, s.lstrip("/").strip())
    for k, s in enumerate(block):
        if s.lower().startswith("translators:"):
            return " ".join(block[k:])
    return ""


# ── Catalog ───────────────────────────────────────────────────────────────────


def collect(domain):
    """{(context, msgid): entry} in first-seen order, plus problems found on the way."""
    crate = DOMAINS[domain]
    entries, problems = {}, []
    files = list(sources(crate, ".slint")) + list(sources(crate, ".rs"))
    counts = {"slint": 0, "rust": 0}
    for path in files:
        rust = path.endswith(".rs")
        rel = os.path.relpath(path, crate).replace(os.sep, "/")
        for line, ctx, msgid, plural, note in calls(path, rust):
            counts["rust" if rust else "slint"] += 1
            e = entries.setdefault((ctx, msgid), {"ctx": ctx, "id": msgid, "plural": plural, "refs": [], "notes": []})
            if e["plural"] != plural:
                problems.append(f"{rel}:{line}: {msgid!r} used with two different plurals ({e['plural']!r}, {plural!r})")
            e["refs"].append(f"{rel}:{line}")
            if note and note not in e["notes"]:
                e["notes"].append(note)
    return list(entries.values()), counts, problems


def quote(s):
    s = s.replace("\\", "\\\\").replace('"', '\\"').replace("\t", "\\t").replace("\r", "\\r")
    if "\n" not in s:
        return '"' + s.replace("\n", "\\n") + '"'
    # Multi-line text: one source line per po line, after an empty first string.
    parts = s.split("\n")
    lines = ['""'] + ['"' + p + '\\n"' for p in parts[:-1]] + (['"' + parts[-1] + '"'] if parts[-1] else [])
    return "\n".join(lines)


def write_po(path, domain, entries, header_extra, translate):
    out = [
        f"# {domain}: every string Lightify shows, the .slint (@tr) and Rust (tr!/trn!) halves.",
        "# Generated by scripts/extract-strings.py; edit the source, not this file.",
        "# {} {0} {n} are placeholders: keep each one, as written, in the translation.",
        'msgid ""',
        'msgstr ""',
        f'"Project-Id-Version: {domain}\\n"',
        '"MIME-Version: 1.0\\n"',
        '"Content-Type: text/plain; charset=UTF-8\\n"',
        '"Content-Transfer-Encoding: 8bit\\n"',
    ] + [f'"{h}\\n"' for h in header_extra]
    for e in entries:
        out.append("")
        # Notes for translators (`// Translators: …` above the call), as gettext writes them.
        for note in e.get("notes", []):
            out.append("#. " + note)
        # gettext wraps references; keep lines under ~80 columns the same way.
        line = "#:"
        for r in e["refs"]:
            if len(line) + 1 + len(r) > 79 and line != "#:":
                out.append(line)
                line = "#:"
            line += " " + r
        out.append(line)
        if e["ctx"]:
            out.append("msgctxt " + quote(e["ctx"]))
        out.append("msgid " + quote(e["id"]))
        if e["plural"] is not None:
            out.append("msgid_plural " + quote(e["plural"]))
            out.append("msgstr[0] " + quote(translate(e["id"])))
            out.append("msgstr[1] " + quote(translate(e["plural"])))
        else:
            out.append("msgstr " + quote(translate(e["id"])))
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(out) + "\n")


# ── Pseudo language ───────────────────────────────────────────────────────────

# Accents every Latin-script catalog the plan lists already needs (Space Grotesk draws them).
ACCENTS = str.maketrans(
    "acdegilnorstuyzACDEGILNORSTUYZ",
    "áçďéğíłñöřšťüýžÅÇĎÉĞÍŁÑÖŘŠŤÜÝŽ",
)
PLACEHOLDER = re.compile(r"\{\{|\}\}|\{(?:\d*|n)\}")


def pseudo(s):
    """`Settings` → `[Šéťťíñğš ····]`: accented, ~40 % longer, bracketed so a cut-off end
    shows. Placeholders stay exactly as written; text with no letters (arrows, ×) is left
    alone, as a translator would."""
    plain = PLACEHOLDER.sub("", s)
    if not re.search(r"[A-Za-z]", plain):
        return s
    out, last = [], 0
    for m in PLACEHOLDER.finditer(s):
        out.append(s[last:m.start()].translate(ACCENTS))
        out.append(m.group(0))
        last = m.end()
    out.append(s[last:].translate(ACCENTS))
    pad = "\u00b7" * max(1, math.ceil(len(s) * 0.4))
    return "[" + "".join(out) + " " + pad + "]"


# ── Check ─────────────────────────────────────────────────────────────────────


def template_ids(path):
    """(context, msgid, plural) of every entry in an existing template."""
    ids, cur = set(), {}
    text = open(path, encoding="utf-8").read() if os.path.exists(path) else ""
    for block in text.split("\n\n"):
        cur, field = {}, None
        for line in block.splitlines():
            m = re.match(r'(msgctxt|msgid_plural|msgid|msgstr(?:\[\d+\])?) "(.*)"$', line)
            if m:
                field = m.group(1)
                cur[field] = m.group(2)
            elif line.startswith('"') and field:
                cur[field] += line[1:-1]
        if cur.get("msgid"):
            ids.add((unquote(cur.get("msgctxt", "")), unquote(cur["msgid"]), unquote(cur.get("msgid_plural"))))
    return ids


def unquote(s):
    """`quote`'s escapes undone."""
    if s is None:
        return None
    esc = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\"}
    return re.sub(r"\\(.)", lambda m: esc.get(m.group(1), "\\" + m.group(1)), s)


def check():
    """Is each template up to date with the source? Lists what a regenerate would add
    and remove (a changed string shows as one of each); 1 when anything differs."""
    stale = False
    for domain, crate in DOMAINS.items():
        entries, _, problems = collect(domain)
        for p in problems:
            print("  problem:", p)
            stale = True
        path = os.path.join(crate, "translations", domain + ".pot")
        have = template_ids(path)
        want = {(e["ctx"], e["id"], e["plural"]) for e in entries}
        label = lambda k: (f"[{k[0]}] " if k[0] else "") + repr(k[1]) + (f" | plural {k[2]!r}" if k[2] else "")
        for k in sorted(want - have, key=label):
            print(f"{domain}: + {label(k)}")
        for k in sorted(have - want, key=label):
            print(f"{domain}: - {label(k)}")
        if want != have:
            stale = True
            print(f"{os.path.relpath(path, ROOT)} is out of date: run python scripts/extract-strings.py")
        else:
            print(f"{os.path.relpath(path, ROOT)}: up to date ({len(want)} strings)")
    return 1 if stale else 0


# ── Main ──────────────────────────────────────────────────────────────────────


def main(argv):
    if len(argv) == 3 and argv[0] == "--pseudo":
        domain, out = argv[1], argv[2]
        entries, _, problems = collect(domain)
        if problems:
            sys.exit("\n".join(problems))
        write_po(out, domain, entries, ["Language: qps-ploc", "Plural-Forms: nplurals=2; plural=(n != 1);"], pseudo)
        return
    if argv == ["--check"]:
        sys.exit(check())
    if argv and not all(a in DOMAINS for a in argv):
        sys.exit(__doc__)
    failed = False
    for domain, crate in DOMAINS.items():
        if argv and domain not in argv:
            continue
        entries, counts, problems = collect(domain)
        for p in problems:
            print("  problem:", p)
            failed = True
        path = os.path.join(crate, "translations", domain + ".pot")
        write_po(path, domain, entries, ["Plural-Forms: nplurals=INTEGER; plural=EXPRESSION;"], lambda s: "")
        plurals = sum(1 for e in entries if e["plural"] is not None)
        print(
            f"{os.path.relpath(path, ROOT)}: {len(entries)} strings ({plurals} plural) from "
            f"{counts['slint']} @tr + {counts['rust']} tr!/trn!/tr_noop! calls"
        )
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main(sys.argv[1:])
