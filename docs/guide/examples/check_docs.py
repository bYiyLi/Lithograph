#!/usr/bin/env python3
"""Check public Markdown links, anchors and the frozen-profile coverage map."""

import argparse
from collections import Counter
import html
import json
from pathlib import Path
import re
from urllib.parse import unquote, urlsplit

from doc_runtime import PROFILE, ROOT
from verify_cypher import read_blocks, read_examples


def prose(text):
    lines = []
    fence = None
    for line in text.splitlines():
        start = re.match(r"^\s*(`{3,}|~{3,})", line)
        if start:
            if fence is None:
                fence = start[1][0]
            elif start[1][0] == fence:
                fence = None
            lines.append("")
        else:
            lines.append(line if fence is None else "")
    return "\n".join(lines)


def anchors(path):
    text = prose(path.read_text())
    values = set(re.findall(r"\b(?:id|name)=[\"']([^\"']+)[\"']", text))
    counts = Counter()
    for heading in re.findall(r"^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$", text, re.M):
        heading = re.sub(r"\[([^]]+)\]\([^)]*\)", r"\1", heading)
        heading = re.sub(r"<[^>]*>", "", heading)
        heading = html.unescape(heading).lower()
        slug = re.sub(r"[^\w\-\s]", "", heading).replace(" ", "-")
        slug = re.sub(r"\s", "-", slug)
        suffix = "-" + str(counts[slug]) if counts[slug] else ""
        values.add(slug + suffix)
        counts[slug] += 1
    return values


def local_target(source, target):
    target = html.unescape(target.strip().strip("<>"))
    parsed = urlsplit(target)
    if parsed.scheme or parsed.netloc or target.startswith("//"):
        return None
    if not parsed.path:
        path = source
    elif parsed.path.startswith("/"):
        path = ROOT / unquote(parsed.path.lstrip("/"))
    else:
        path = source.parent / unquote(parsed.path)
    return path.resolve(), unquote(parsed.fragment)


def check_link(path, target, errors, cache):
    resolved = local_target(path, target)
    if not resolved:
        return
    destination, anchor = resolved
    location = str(path.relative_to(ROOT))
    if not destination.exists():
        errors.append(location + ": missing link target " + target)
    elif anchor and destination.is_file() and destination.suffix.lower() == ".md":
        if destination not in cache:
            cache[destination] = anchors(destination)
        if anchor not in cache[destination]:
            errors.append(location + ": missing anchor " + target)


def markdown_links(path, errors, cache):
    text = prose(path.read_text())
    # Inline code can contain syntax-shaped examples which are not actual links.
    text = re.sub(r"`+[^`\n]*`+", "", text)
    pattern = r"!?\[[^]\n]*\]\(\s*(<[^>]+>|[^\s)]+)(?:\s+[^)]*)?\)"
    for match in re.finditer(pattern, text):
        check_link(path, match[1], errors, cache)
    definitions = dict(re.findall(r"^\s{0,3}\[([^]]+)\]:\s*(\S+)", text, re.M))
    for target in definitions.values():
        check_link(path, target, errors, cache)


def table_rows(text):
    rows = []
    header = True
    for line in text.splitlines():
        if not line.startswith("| "):
            continue
        cells = [cell.strip() for cell in re.split(r"(?<!\\)\|", line)[1:-1]]
        if all(re.fullmatch(r":?-+:?", cell) for cell in cells):
            continue
        if header:
            header = False
            continue
        rows.append(cells)
    return rows


def coverage_contract():
    """Return exact capability subitems and (section, item) inventory identities."""
    text = (ROOT / "docs/development/cypher25-compatibility.md").read_text()
    headings = list(re.finditer(r"^#{2,3} (.+)$", text, re.M))
    sections = {heading[1]: text[heading.end():headings[index + 1].start()]
                if index + 1 < len(headings) else text[heading.end():]
                for index, heading in enumerate(headings)}
    capabilities = {row[0]: [item.strip() for item in row[1].split("、")]
                    for row in table_rows(sections["4. Capability Matrix"])}
    inventories = []
    delta = [name for name in sections if name.startswith("5. ") and "Delta Inventory" in name]
    if len(delta) != 1:
        raise RuntimeError("Expected one section 5 Cypher 25 Delta Inventory")
    delta_rows = table_rows(sections[delta[0]])
    delta_items = [row[0] for row in delta_rows] if delta_rows else re.findall(
        r"^- (.+)$", sections[delta[0]], re.M
    )
    inventories.extend((delta[0], item) for item in delta_items)
    for phase in (12, 13, 15):
        names = [name for name in sections if name.startswith("Phase " + str(phase) + " ")
                 and name.endswith(" supplemental inventory")]
        if len(names) != 1:
            raise RuntimeError("Expected one Phase " + str(phase) + " supplemental inventory")
        inventories.extend((names[0], row[0]) for row in table_rows(sections[names[0]]))
    if not capabilities or not delta_items:
        raise RuntimeError("Compatibility source contains an empty documentation denominator")
    return capabilities, inventories


def matrix_families():
    return list(coverage_contract()[0])


def check_evidence(entry, label, examples, errors, inventory=False):
    identifiers = entry.get("examples", [])
    for example in identifiers:
        if example not in examples:
            errors.append(label + ": unknown example " + example)
    if inventory and entry.get("applicability") == "superseded":
        if not isinstance(entry.get("reason"), str) or not entry["reason"].strip():
            errors.append(label + ": superseded item needs a reason")
        if not isinstance(entry.get("replacement"), str) or not entry["replacement"].strip():
            errors.append(label + ": superseded item needs a current replacement reference")
        return
    if inventory and entry.get("applicability", "applicable") != "applicable":
        errors.append(label + ": unknown applicability")
    verification = entry.get("verification") if inventory else None
    if verification is not None:
        if not isinstance(verification, dict) or not all(
            isinstance(verification.get(key), str) and verification[key].strip()
            for key in ("command", "reason")
        ):
            errors.append(label + ": verification needs a command and reason")
    if not identifiers and verification is None:
        errors.append(label + ": no executable example evidence" +
                      (" or non-Cypher verification" if inventory else ""))


def check_coverage(path, errors, cache):
    document = json.loads(path.read_text())
    if document.get("profile") != PROFILE:
        errors.append("Coverage profile differs from " + PROFILE)
    entries = document.get("capabilities", [])
    names = [entry["family"] for entry in entries]
    capabilities, inventories = coverage_contract()
    expected = set(capabilities)
    for family in sorted(expected - set(names)):
        errors.append("Missing documentation coverage for capability: " + family)
    for family in sorted(set(names) - expected):
        errors.append("Unknown capability in coverage map: " + family)
    for family, count in Counter(names).items():
        if count > 1:
            errors.append("Duplicate capability in coverage map: " + family)
    blocks, block_errors = read_blocks()
    examples, example_errors = read_examples(blocks)
    errors.extend(block_errors + example_errors)
    nested = []
    for entry in entries:
        family = entry["family"]
        check_evidence(entry, family, examples, errors)
        required = entry.get("required", [])
        items = [item["item"] for item in required]
        expected_items = set(capabilities.get(family, []))
        for item in sorted(expected_items - set(items)):
            errors.append(family + ": missing required coverage item " + item)
        for item in sorted(set(items) - expected_items):
            errors.append(family + ": unknown required coverage item " + item)
        for item, count in Counter(items).items():
            if count > 1:
                errors.append(family + ": duplicate required coverage item " + item)
        for item in required:
            check_evidence(item, family + " / " + item["item"], examples, errors)
        nested.extend(required)
    mapped_inventories = document.get("inventories", [])
    inventory_keys = [(entry["section"], entry["item"]) for entry in mapped_inventories]
    for section, item in sorted(set(inventories) - set(inventory_keys)):
        errors.append("Missing inventory coverage: " + section + " / " + item)
    for section, item in sorted(set(inventory_keys) - set(inventories)):
        errors.append("Unknown inventory coverage: " + section + " / " + item)
    for (section, item), count in Counter(inventory_keys).items():
        if count > 1:
            errors.append("Duplicate inventory coverage: " + section + " / " + item)
    for entry in mapped_inventories:
        label = entry["section"] + " / " + entry["item"]
        check_evidence(entry, label, examples, errors, inventory=True)
        if entry.get("applicability") == "superseded" and isinstance(entry.get("replacement"), str):
            check_link(ROOT / "README.md", entry["replacement"], errors, cache)
    for entry in entries + nested + mapped_inventories + document.get("surfaces", []):
        if not isinstance(entry.get("path"), str) or not entry["path"].strip():
            errors.append("Coverage item has no documentation path: " + str(entry.get("item", entry)))
            continue
        target = entry["path"]
        anchor = entry.get("anchor")
        if anchor:
            target += "#" + anchor
        check_link(ROOT / "README.md", target, errors, cache)
        for extra in entry.get("anchors", []):
            check_link(ROOT / "README.md", entry["path"] + "#" + extra, errors, cache)
    # Derive registered SQL names rather than maintaining another API denominator.
    scalar = (ROOT / "crates/lithograph-extension/src/scalar.rs").read_text()
    registered = set(re.findall(r'register_scalar\(\s*db,\s*c"(lithograph\w*)"', scalar))
    registered.add("lithograph_rows")
    surface_names = {entry["name"] for entry in document.get("surfaces", []) if entry["kind"] == "sql"}
    for name in sorted(registered - surface_names):
        errors.append("Public SQL API has no coverage entry: " + name)
    options = (ROOT / "crates/lithograph-core/src/query/options.rs").read_text()
    validation = options.split("fn validate_top_level_keys(", 1)[1].split("fn optional_nonempty_string(", 1)[0]
    option_names = set(re.findall(r'"([a-zA-Z][a-zA-Z0-9]*)"', validation))
    option_names.add("expectedHead")
    documented_options = {entry["name"].removeprefix("options.") for entry in document.get("surfaces", [])
                          if entry["kind"] == "option"}
    for name in sorted(option_names - documented_options):
        errors.append("Public execution option has no coverage entry: " + name)
    config = (ROOT / "crates/lithograph-openai-compatible/src/config.rs").read_text()
    provider_fields = set()
    for name, prefix in (("ProviderConfig", ""), ("CacheConfig", "cache.")):
        body = config.split("struct " + name + " {", 1)[1].split("\n}", 1)[0]
        provider_fields.update(prefix + field for field in re.findall(r"pub\(crate\) (\w+):", body))
    documented_fields = {entry["name"].removeprefix("providerConfig.")
                         for entry in document.get("surfaces", []) if entry["kind"] == "provider-config"}
    for name in sorted(provider_fields - documented_fields):
        errors.append("Public Provider configuration has no coverage entry: " + name)
    print("Coverage:", len(entries), "capability families,", len(nested), "required items,",
          len(mapped_inventories), "delta/supplement items,",
          len(document.get("surfaces", [])), "public surfaces")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=Path)
    parser.add_argument("--coverage", type=Path,
                        default=ROOT / "docs/development/documentation-coverage.json")
    args = parser.parse_args()
    candidates = args.paths or [ROOT / "README.md", ROOT / "docs/guide",
                                ROOT / "docs/reference", ROOT / "docs/cypher", ROOT / "docs/releases"]
    files = set()
    for candidate in candidates:
        candidate = candidate.resolve()
        files.update(candidate.rglob("*.md") if candidate.is_dir() else [candidate])
    errors = []
    cache = {}
    for path in sorted(files):
        markdown_links(path, errors, cache)
    if args.coverage.exists():
        check_coverage(args.coverage, errors, cache)
    else:
        errors.append("Missing documentation coverage map: " + str(args.coverage))
    if errors:
        raise SystemExit("\n".join(errors))
    print("PASS:", len(files), "Markdown files; local links, anchors and coverage")


if __name__ == "__main__":
    main()
