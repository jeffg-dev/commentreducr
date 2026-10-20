"""Train/dev/test split by id hash: the one implementation, import split_of everywhere."""

import hashlib
import json


def split_of(item_id: str) -> str:
    b = int(hashlib.sha1(item_id.encode("utf-8")).hexdigest(), 16) % 10
    return "train" if b < 5 else "dev" if b < 7 else "test"


if __name__ == "__main__":
    # python3 split.py test < gold.jsonl > gold-test.jsonl
    import sys

    for line in sys.stdin:
        if line.strip() and split_of(json.loads(line)["id"]) == sys.argv[1]:
            sys.stdout.write(line)
