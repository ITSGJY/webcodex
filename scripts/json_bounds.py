"""Explicit JSON parsing limits shared by offline Python tooling."""

from __future__ import annotations

import json
from typing import Any

MAX_NESTING = 256
MAX_INTEGER_DIGITS = 4300


def _bounded_integer(token: str) -> int:
    if len(token.removeprefix("-")) > MAX_INTEGER_DIGITS:
        raise ValueError("JSON integer exceeds bounded digits")
    return int(token)


def loads(text: str) -> Any:
    # Interpreter recursion guards vary. Bound JSON depth before allocating
    # the decoded tree, ignoring punctuation inside escaped JSON strings.
    depth = 0
    in_string = False
    escaped = False
    for char in text:
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        elif char == '"':
            in_string = True
        elif char in "[{":
            depth += 1
            if depth > MAX_NESTING:
                raise ValueError("JSON nesting exceeds bounded depth")
        elif char in "]}":
            depth -= 1

    # Structural and Unicode validation remain with Python's JSON decoder.
    return json.loads(text, parse_int=_bounded_integer)
