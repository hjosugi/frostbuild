#!/usr/bin/env python3
import hashlib, os, sys, time
time.sleep(int(os.environ.get('FROST_BAZEL_SLEEP_MS', '20')) / 1000)
h = hashlib.sha256()
for p in sys.argv[1:]:
    with open(p, 'rb') as f: h.update(f.read())
print(h.hexdigest())
