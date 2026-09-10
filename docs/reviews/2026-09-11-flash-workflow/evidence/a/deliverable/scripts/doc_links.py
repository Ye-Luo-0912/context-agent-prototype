#!/usr/bin/env python3
"""Markdown link validation for the document-consistency gate.

Every relative markdown link in the given live documents must resolve on
disk. External links (http/https/mailto) and same-page fragments are
ignored.
"""

import os
import re
import urllib.parse

LINK_PATTERN = re.compile(r"\]\(([^)\s#]+)(?:#[^)\s]*)?\)")


def check_markdown_links(root, relative_paths, violations):
    """Append a violation for every broken relative link found.

    ``root`` is the repository root, ``relative_paths`` the live documents
    to scan (relative to ``root``), and ``violations`` the list that broken
    links (and missing documents) are appended to.
    """
    for relative in relative_paths:
        path = os.path.join(root, relative)
        if not os.path.isfile(path):
            violations.append(f"live document missing: {relative}")
            continue
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        base = os.path.dirname(path)
        for match in LINK_PATTERN.finditer(text):
            target = match.group(1)
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            resolved = os.path.normpath(
                os.path.join(base, urllib.parse.unquote(target))
            )
            if not os.path.exists(resolved):
                violations.append(f"{relative}: broken link -> {target}")
