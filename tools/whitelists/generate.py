#!/usr/bin/env python3

import ipaddress
import json
from pathlib import Path
import sys
import tomllib
import urllib.request


def get_json(url: str) -> dict:
    res = urllib.request.urlopen(url)
    body = res.read().decode('utf-8')
    return json.loads(body)


def lookup_strings(data: dict, pattern: str):
    parts = pattern.split('.')
    result = []

    def recurse(data, i: int):
        if type(data) == str:
            if i == len(parts):
                result.append(data)
        elif type(data) == dict:
            values = (v for k, v in data.items() if parts[i] == "*" or parts[i] == k)
            for val in values:
                recurse(val, i + 1)
        elif type(data) == list:
            values = (v for idx, v in enumerate(data) if parts[i] == "*" or int(parts[i]) == idx)
            for val in values:
                recurse(val, i + 1)

    recurse(data, 0)
    return result


def generate(config: Path, output: Path):
    with open(config, "rb") as f:
        settings = tomllib.load(f)

    blocks = []

    def read_blocks(url, pattern):
        data = get_json(url)
        blocks.extend(lookup_strings(data, pattern))

    if 'url' in settings and 'pattern' in settings:
        read_blocks(settings["url"], settings["pattern"])
    for val in settings.values():
        if type(val) == dict and 'url' in val and 'pattern' in val:
            read_blocks(val["url"], val["pattern"])

    # Parse CIDR blocks and put them in a sensible order
    networks = sorted(
        {ipaddress.ip_network(b) for b in blocks},
        key=lambda n: (n.version, n.network_address, n.prefixlen),
    )

    with open(output, "w") as f:
        print(f"# {config}", file=f)
        print("cidrs = [", file=f)
        for net in networks:
            print(f'  "{net}",', file=f)
        print("]", file=f)


configs = sys.argv[1:]
output_dir = Path(__file__).parent / "output"
for config in configs:
    input = Path(config)
    output = output_dir / input.name
    print(f"Processing {input} -> {output}")
    generate(input, output)
