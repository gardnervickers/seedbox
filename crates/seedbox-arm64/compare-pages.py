#!/usr/bin/env python3
"""Locate differing power-off pages; this does not replace witness comparison."""
import json
import sys


def compare(a, b):
    for key in ('format', 'page_size', 'seed', 'kernel_sha256', 'initrd_sha256'):
        if a[key] != b[key]:
            raise ValueError(f'NOT_COMPARABLE: {key}')
    if a['format'] != 1 or a['page_size'] != 4096:
        raise ValueError('unsupported page diagnostic format')
    layout = lambda doc: [(r['name'], r['base'], r['length'], len(r['sha256']))
                          for r in doc['regions']]
    if layout(a) != layout(b):
        raise ValueError('NOT_COMPARABLE: memory layout')
    count = 0
    for left, right in zip(a['regions'], b['regions']):
        if left['length'] != len(left['sha256']) * a['page_size']:
            raise ValueError('invalid page count')
        for i, (x, y) in enumerate(zip(left['sha256'], right['sha256'])):
            if x != y:
                offset = i * a['page_size']
                print(f"{left['name']} page={i} offset={offset:#x} "
                      f"gpa={left['base'] + offset:#x}: {x} / {y}")
                count += 1
    print(f'differing pages: {count}')
    return bool(count)


def main():
    if len(sys.argv) != 3:
        raise ValueError(f'usage: {sys.argv[0]} LEFT.pages.json RIGHT.pages.json')
    with open(sys.argv[1]) as f:
        a = json.load(f)
    with open(sys.argv[2]) as f:
        b = json.load(f)
    return int(compare(a, b))


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError, TypeError) as error:
        print(error, file=sys.stderr)
        sys.exit(2)
