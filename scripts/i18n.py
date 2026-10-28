"""Translation catalogs of the desktop app.

Every app text is written where it is used as a Turkish/English pair: `ui("…", "…")` for a fixed
text, `ui_format!("…", "…", args…)` for a template. The other interface languages translate the
English text in `i18n/<code>.json`, a JSON object from English text to translation.

    python scripts/i18n.py extract   writes target/i18n-source.json: every English text with its
                                     Turkish wording and where it is used, for translators
    python scripts/i18n.py build     compiles i18n/*.json into src/i18n_catalog.rs
    python scripts/i18n.py check     fails on an untranslated or stale catalog entry, a
                                     placeholder or menu marker a translation changes, two items
                                     of one menu that share an access letter in any language, a
                                     text the extractor cannot read, or an out-of-date
                                     i18n_catalog.rs
"""
import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SOURCE = ROOT / "src"
CATALOGS = ROOT / "i18n"
GENERATED = SOURCE / "i18n_catalog.rs"
LANGUAGES = [("ar", "Arabic"), ("es", "Spanish"), ("pt", "Portuguese"), ("fr", "French"), ("de", "German")]

ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "\\": "\\", '"': '"', "'": "'", "0": "\0"}


def is_string_start(text, index):
    if text[index] == '"':
        return True
    return text[index] == "r" and text[index + 1:index + 2] in ('"', "#") and not (index and (text[index - 1].isalnum() or text[index - 1] == "_"))


def skip_string(text, index):
    """Index just past the string literal (plain or raw) that starts at `index`."""
    if text[index] == "r":
        hashes = 0
        index += 1
        while text[index] == "#":
            hashes += 1
            index += 1
        end = text.index('"' + "#" * hashes, index + 1)
        return end + 1 + hashes
    index += 1
    while text[index] != '"':
        index += 2 if text[index] == "\\" else 1
    return index + 1


def skip_char(text, index):
    """Index past a char literal at `index`, or `index` itself for a lifetime."""
    match = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'])'", text[index:])
    return index + match.end() if match else index


def closing(text, start):
    """Index of the bracket that closes the one just before `start`."""
    depth, index = 1, start
    while index < len(text):
        char = text[index]
        if is_string_start(text, index):
            index = skip_string(text, index)
            continue
        if char == "'":
            after = skip_char(text, index)
            if after != index:
                index = after
                continue
        if text.startswith("//", index):
            index = text.index("\n", index)
            continue
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    raise ValueError("unbalanced brackets")


def split_top(text):
    """The comma-separated parts of `text` outside brackets and strings."""
    parts, depth, index, start = [], 0, 0, 0
    while index < len(text):
        char = text[index]
        if is_string_start(text, index):
            index = skip_string(text, index)
            continue
        if char == "'":
            after = skip_char(text, index)
            if after != index:
                index = after
                continue
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        elif char == "," and depth == 0:
            parts.append(text[start:index].strip())
            start = index + 1
        index += 1
    if text[start:].strip():
        parts.append(text[start:].strip())
    return parts


def literal_value(spelling):
    """The value of a Rust string literal, or None when `spelling` is not one."""
    spelling = spelling.strip()
    raw = re.fullmatch(r'r(#*)"(.*)"\1', spelling, re.S)
    if raw:
        return raw.group(2)
    if not re.fullmatch(r'"(?:[^"\\]|\\.)*"', spelling, re.S):
        return None
    body, out, index = spelling[1:-1], [], 0
    while index < len(body):
        char = body[index]
        if char != "\\":
            out.append(char)
            index += 1
            continue
        code = body[index + 1]
        if code in ESCAPES:
            out.append(ESCAPES[code])
            index += 2
        elif code in "\r\n":
            index += 1
            while index < len(body) and body[index] in " \t\r\n":
                index += 1
        elif code == "u":
            end = body.index("}", index)
            out.append(chr(int(body[index + 3:end], 16)))
            index = end + 1
        elif code == "x":
            out.append(chr(int(body[index + 2:index + 4], 16)))
            index += 4
        else:
            raise ValueError(f"unknown escape \\{code}")
    return "".join(out)


def rust_string(value):
    """The one spelling a template is matched by: only these characters are escaped."""
    out = ['"']
    for char in value:
        if char == "\\":
            out.append("\\\\")
        elif char == '"':
            out.append('\\"')
        elif char == "\n":
            out.append("\\n")
        elif char == "\r":
            out.append("\\r")
        elif char == "\t":
            out.append("\\t")
        elif ord(char) < 0x20 or ord(char) == 0x7F:
            out.append("\\u{%x}" % ord(char))
        else:
            out.append(char)
    out.append('"')
    return "".join(out)


PLACEHOLDER = re.compile(r"\{\{|\}\}|\{[^{}]*\}")


def placeholders(template):
    return sorted(match for match in PLACEHOLDER.findall(template) if match not in ("{{", "}}"))


def code_only(text):
    """`text` with comments blanked out, so commented code is not read as texts."""
    out, index = [], 0
    while index < len(text):
        if is_string_start(text, index):
            end = skip_string(text, index)
            out.append(text[index:end])
            index = end
        elif text.startswith("//", index):
            end = text.find("\n", index)
            end = len(text) if end < 0 else end
            out.append(" " * (end - index))
            index = end
        elif text.startswith("/*", index):
            end = text.index("*/", index) + 2
            out.append(re.sub(r"[^\n]", " ", text[index:end]))
            index = end
        elif text[index] == "'":
            end = skip_char(text, index)
            end = index + 1 if end == index else end
            out.append(text[index:end])
            index = end
        else:
            out.append(text[index])
            index += 1
    return "".join(out)


class Extraction:
    def __init__(self):
        self.entries = {}
        self.problems = []

    def add(self, en, tr, kind, where):
        entry = self.entries.setdefault(en, {"en": en, "tr": [], "kinds": set(), "where": []})
        if tr is not None and tr not in entry["tr"]:
            entry["tr"].append(tr)
        entry["kinds"].add(kind)
        entry["where"].append(where)


def consts(text):
    """`const NAME: &str = "…";` values of a file."""
    found = {}
    for match in re.finditer(r"const\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*", text):
        end = skip_string(text, match.end()) if is_string_start(text, match.end()) else None
        if end:
            found[match.group(1)] = literal_value(text[match.end():end])
    return found


def table_pairs(text, name, tr_name, en_name, pattern):
    """The (tr, en) pairs of the const array `name`, at the positions `pattern` gives them."""
    names = [part.strip().lstrip("&") for part in split_top(pattern)]
    if tr_name not in names or en_name not in names:
        return None
    match = re.search(r"const\s+" + name + r"\s*:[^=]*=\s*\[", text)
    if not match:
        return None
    body = text[match.end():closing(text, match.end())]
    pairs = []
    for row in split_top(body):
        fields = split_top(row.strip()[1:-1]) if row.strip().startswith("(") else None
        if not fields or len(fields) != len(names):
            return None
        pairs.append((literal_value(fields[names.index(tr_name)]), literal_value(fields[names.index(en_name)])))
    return pairs


def extract():
    result = Extraction()
    helpers = set()
    files = {}
    for path in sorted(SOURCE.rglob("*.rs")):
        if path == GENERATED:
            continue
        files[path] = code_only(path.read_text(encoding="utf-8"))
        # A function or closure that takes a pair and hands it to ui() is read like ui().
        for match in re.finditer(r"(?:fn\s+([a-z_][a-z0-9_]*)\s*\(|let\s+([a-z_][a-z0-9_]*)\s*=\s*\|)\s*tr\s*:\s*&'static\s+str\s*,\s*en\s*:\s*&'static\s+str", files[path]):
            helpers.add(match.group(1) or match.group(2))
    callers = "|".join(sorted({"ui"} | helpers))
    for path, text in files.items():
        relative = path.relative_to(ROOT).as_posix()
        local_consts = consts(text)
        for match in re.finditer(r"(?<![\w.:$])(?:(?:crate|ssdownload)::i18n::|i18n::)?(" + callers + r"|ui_format!)\s*\(", text):
            if text[max(0, match.start() - 3):match.start()] == "fn " or text[match.start() - 1:match.start()] == "|":
                continue
            where = f"{relative}:{text[:match.start()].count(chr(10)) + 1}"
            arguments = split_top(text[match.end():closing(text, match.end())])
            if match.group(1) == "ui_format!":
                if len(arguments) < 2:
                    result.problems.append(f"{where}: ui_format! needs a Turkish and an English template")
                    continue
                tr, en = literal_value(arguments[0]), literal_value(arguments[1])
                if tr is None or en is None:
                    result.problems.append(f"{where}: ui_format! templates must be string literals")
                    continue
                if rust_string(en) != arguments[1].strip():
                    result.problems.append(f"{where}: spell the English template as {rust_string(en)} so its translations match")
                result.add(en, tr, "template", where)
                continue
            if len(arguments) != 2:
                continue
            tr = literal_value(arguments[0])
            en = literal_value(arguments[1])
            if tr is None and arguments[0] in local_consts:
                tr = local_consts[arguments[0]]
            if en is None and arguments[1] in local_consts:
                en = local_consts[arguments[1]]
            if en is not None:
                result.add(en, tr, "text", where)
                continue
            names = (arguments[0], arguments[1])
            if names == ("tr", "en") and match.group(1) != "ui":
                continue
            loop = None
            for candidate in re.finditer(r"for\s+(?:\(\s*\w+\s*,\s*)?\(([^()]*)\)\)?\s+in\s+([A-Z][A-Z0-9_]*)", text[:match.start()]):
                loop = candidate
            pairs = table_pairs(text, loop.group(2), *names, loop.group(1)) if loop else None
            if names == ("tr", "en") and pairs is None:
                # A helper's own body: its callers were read above.
                if re.search(r"(?:fn\s+\w+\s*\(|\|)\s*tr\s*:\s*&'static\s+str\s*,\s*en\s*:", text[max(0, match.start() - 400):match.start()]):
                    continue
            if pairs is None or any(en is None for _, en in pairs):
                if (relative, names) != ("src/error_codes.rs", ("entry.message_tr", "entry.message_en")):
                    result.problems.append(f"{where}: ui({', '.join(names)}) reads texts the extractor cannot find")
                continue
            for tr, en in pairs:
                result.add(en, tr, "text", where)
    # The error code registry rows: code, area, stage, Turkish, English, description.
    registry = files[SOURCE / "error_codes.rs"]
    body = registry[registry.index("define! {"):]
    for row in re.finditer(r"^\s*[A-Z]{3}_\d{3}\s*=>(.*);\s*$", body, re.M):
        strings = [literal_value(part) for part in split_top(row.group(1)) if literal_value(part) is not None]
        line = registry[:registry.index(row.group(0))].count("\n") + 1
        result.add(strings[4], strings[3], "text", f"src/error_codes.rs:{line}")
    return result


def load_catalog(code):
    path = CATALOGS / f"{code}.json"
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}


def utf8(value):
    return value.encode("utf-8")


def generate(extraction):
    lines = [
        "//! Generated by `scripts/i18n.py build` from `i18n/*.json`; edit the catalogs, not this file.",
        "",
        "use crate::i18n::Language;",
        "",
        "/// The fixed texts a language translates, sorted by their English text.",
        "pub(crate) fn texts(language: Language) -> &'static [(&'static str, &'static str)] {",
        "    match language {",
    ]
    for code, name in LANGUAGES:
        lines.append(f"        Language::{name} => {code.upper()},")
    lines += ["        Language::Turkish | Language::English => &[],", "    }", "}", ""]
    for code, _ in LANGUAGES:
        catalog = load_catalog(code)
        texts = sorted(
            ((en, translation) for en, translation in catalog.items()
             if en in extraction.entries and "text" in extraction.entries[en]["kinds"]),
            key=lambda pair: utf8(pair[0]),
        )
        if not texts:
            lines += [f"static {code.upper()}: &[(&str, &str)] = &[];", ""]
            continue
        lines.append(f"static {code.upper()}: &[(&str, &str)] = &[")
        for en, translation in texts:
            lines.append(f"    ({rust_string(en)}, {rust_string(translation)}),")
        lines += ["];", ""]
    for code, _ in LANGUAGES:
        catalog = load_catalog(code)
        templates = sorted(
            ((en, translation) for en, translation in catalog.items()
             if en in extraction.entries and "template" in extraction.entries[en]["kinds"]),
            key=lambda pair: utf8(pair[0]),
        )
        lines += [
            "#[doc(hidden)]",
            "#[macro_export]",
            f"macro_rules! __ui_template_{code} {{",
        ]
        for en, translation in templates:
            lines.append(f"    ({rust_string(en)}) => {{")
            lines.append(f"        {rust_string(translation)}")
            lines.append("    };")
        lines += ["    ($english:tt) => {", "        $english", "    };", "}", ""]
    # The file is compiled and checked by `cargo fmt --check`, so it is written formatted.
    formatted = subprocess.run(
        ["rustfmt", "--edition", "2021", "--emit", "stdout"],
        input="\n".join(lines).encode("utf-8"), capture_output=True, check=True,
    )
    return formatted.stdout.decode("utf-8").replace("\r\n", "\n")


def mnemonics(text):
    return len(re.findall(r"&(?!&)", text.replace("&&", "")))


def access_letter(text):
    """The letter `&` marks in a menu label, lowercased, or None."""
    match = re.search(r"&(.)", text.replace("&&", ""))
    return match.group(1).casefold() if match else None


def menus():
    """The items of every window menu: {(place, menu): [(line, [(tr, en), ...])]}.

    A menu is the variable `append()` and `AppendMenuW()` add to inside one function; an item is
    one call, and a label chosen by a condition (Pause or Resume) lists each of its pairs."""
    found = {}
    for path in sorted((SOURCE / "gui" / "native").glob("*.rs")):
        text = code_only(path.read_text(encoding="utf-8"))
        functions = [(match.start(), match.group(1)) for match in re.finditer(r"\bfn\s+([a-z_][a-z0-9_]*)", text)]
        for call in re.finditer(r"(?<![\w.])(append|AppendMenuW)\s*\(", text):
            arguments = split_top(text[call.end():closing(text, call.end())])
            label = arguments[2] if call.group(1) == "append" and len(arguments) == 3 else arguments[3] if len(arguments) == 4 else ""
            items = []
            for ui in re.finditer(r"(?:crate::i18n::)?\b(ui|ui_format!)\s*\(", label):
                parts = split_top(label[ui.end():closing(label, ui.end())])
                if len(parts) >= 2 and literal_value(parts[0]) is not None and literal_value(parts[1]) is not None:
                    items.append((literal_value(parts[0]), literal_value(parts[1])))
            if not items:
                continue
            function = next((name for start, name in reversed(functions) if start < call.start()), "")
            place = f"{path.relative_to(ROOT).as_posix()} fn {function}"
            line = text[:call.start()].count("\n") + 1
            found.setdefault((place, arguments[0]), []).append((line, items))
    return found


def menu_letter_clashes(catalogs):
    problems = []
    for (place, menu), items in menus().items():
        for code in ["tr", "en"] + [code for code, _ in LANGUAGES]:
            users = {}
            for line, pairs in items:
                letters = set()
                for tr, en in pairs:
                    text = {"tr": tr, "en": en}.get(code, catalogs.get(code, {}).get(en))
                    if text and access_letter(text):
                        letters.add(access_letter(text))
                for letter in letters:
                    users.setdefault(letter, []).append((line, pairs[0][1]))
            for letter, owners in sorted(users.items()):
                if len(owners) > 1:
                    names = ", ".join(json.dumps(en, ensure_ascii=False) for _, en in owners)
                    problems.append(f"{place}, menu {menu} ({code}): access letter {letter!r} is shared by {names}")
    return problems


def check(extraction):
    problems = list(extraction.problems)
    catalogs = {}
    for code, _ in LANGUAGES:
        catalog = catalogs[code] = load_catalog(code)
        for en in sorted(set(extraction.entries) - set(catalog)):
            problems.append(f"i18n/{code}.json: no translation of {json.dumps(en, ensure_ascii=False)[:120]}")
        for en in sorted(set(catalog) - set(extraction.entries)):
            problems.append(f"i18n/{code}.json: {json.dumps(en, ensure_ascii=False)[:120]} is no longer used")
        for en, translation in catalog.items():
            if en not in extraction.entries:
                continue
            label = f"i18n/{code}.json: {json.dumps(en, ensure_ascii=False)[:80]}"
            if not isinstance(translation, str) or not translation.strip():
                problems.append(f"{label}: empty translation")
                continue
            if placeholders(translation) != placeholders(en):
                problems.append(f"{label}: placeholders {placeholders(translation)} differ from {placeholders(en)}")
            if mnemonics(translation) != mnemonics(en):
                problems.append(f"{label}: the & menu markers differ")
            if ("\t" in en) != ("\t" in translation) or ("\t" in en and en.split("\t", 1)[1] != translation.split("\t", 1)[1]):
                problems.append(f"{label}: the shortcut after the tab must stay {en.split(chr(9), 1)[-1]!r}")
            if en.endswith("\n") != translation.endswith("\n") or en.startswith("\n") != translation.startswith("\n"):
                problems.append(f"{label}: leading or trailing line breaks differ")
    problems += menu_letter_clashes(catalogs)
    if not GENERATED.exists() or GENERATED.read_text(encoding="utf-8") != generate(extraction):
        problems.append("src/i18n_catalog.rs is out of date: run python scripts/i18n.py build")
    return problems


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    extraction = extract()
    if command == "extract":
        target = ROOT / "target" / "i18n-source.json"
        target.parent.mkdir(exist_ok=True)
        entries = [
            {"en": entry["en"], "tr": entry["tr"], "kind": "template" if "template" in entry["kinds"] else "text", "where": entry["where"]}
            for entry in sorted(extraction.entries.values(), key=lambda entry: utf8(entry["en"]))
        ]
        target.write_text(json.dumps(entries, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
        print(f"{len(entries)} texts written to {target}")
        for problem in extraction.problems:
            print(problem)
        return 1 if extraction.problems else 0
    if command == "build":
        GENERATED.write_bytes(generate(extraction).encode("utf-8"))
        print(f"wrote {GENERATED.relative_to(ROOT)}")
        return 0
    if command == "check":
        problems = check(extraction)
        for problem in problems[:200]:
            print(problem)
        if len(problems) > 200:
            print(f"… and {len(problems) - 200} more")
        print(f"{len(extraction.entries)} texts, {len(problems)} problems")
        return 1 if problems else 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
