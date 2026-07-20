"""
FastCDC 2020 chunker — bit-compatible port of Bazel's FastCdcChunker.java.

Source: bazelbuild/bazel @ 3cdd083
  src/main/java/com/google/devtools/build/lib/remote/chunking/FastCdcChunker.java
  src/main/java/com/google/devtools/build/lib/remote/chunking/ChunkingConfig.java

Defaults match Bazel's ChunkingConfig.defaults():
  avg = 512 KiB, min = avg/4 = 128 KiB, max = avg*4 = 2 MiB
  normalization = 2, seed = 0

The two-bytes-per-iteration loop is the FastCDC 2020 optimization
(Xia et al., referenced directly in the REAPI ChunkingFunction enum).
"""

import hashlib
import json
import os

M64 = (1 << 64) - 1

_p = json.load(open(os.path.join(os.path.dirname(__file__), "fastcdc_params.json")))
MASKS = _p["masks"]
GEAR = _p["gear"]
GEAR_LS = [(g << 1) & M64 for g in GEAR]

DEFAULT_AVG = 512 * 1024
DEFAULT_NORMALIZATION = 2
DEFAULT_SEED = 0


class FastCdcChunker:
    def __init__(self, avg_size=DEFAULT_AVG, normalization=DEFAULT_NORMALIZATION,
                 seed=DEFAULT_SEED, min_size=None, max_size=None):
        assert avg_size & (avg_size - 1) == 0, "avgSize must be a power of 2"
        self.avg = avg_size
        self.min = min_size if min_size is not None else avg_size // 4
        self.max = max_size if max_size is not None else avg_size * 4
        bits = avg_size.bit_length() - 1
        small_bits = bits + normalization
        large_bits = bits - normalization
        assert small_bits <= 25 and large_bits >= 5, "normalization too extreme"
        self.mask_s = MASKS[small_bits]
        self.mask_l = MASKS[large_bits]
        self.mask_s_ls = (self.mask_s << 1) & M64
        self.mask_l_ls = (self.mask_l << 1) & M64
        self.seed = seed
        self.seed_ls = (seed << 1) & M64

    def _boundary(self, buf, off, length):
        """Port of FastCdcChunker.findChunkBoundary. Returns chunk length."""
        if length <= self.min:
            return length
        n = min(length, self.max)
        center = min(n, self.avg)
        min_limit = self.min & ~1
        center_limit = center & ~1
        remaining_limit = n & ~1
        s, s_ls = self.seed, self.seed_ls
        h = 0

        for a in range(min_limit, center_limit, 2):
            h = ((h << 2) + (GEAR_LS[buf[off + a]] ^ s_ls)) & M64
            if h & self.mask_s_ls == 0:
                return a
            h = (h + (GEAR[buf[off + a + 1]] ^ s)) & M64
            if h & self.mask_s == 0:
                return a + 1

        for a in range(center_limit, remaining_limit, 2):
            h = ((h << 2) + (GEAR_LS[buf[off + a]] ^ s_ls)) & M64
            if h & self.mask_l_ls == 0:
                return a
            h = (h + (GEAR[buf[off + a + 1]] ^ s)) & M64
            if h & self.mask_l == 0:
                return a + 1

        return n

    def chunk(self, data: bytes):
        """Yield (offset, length) for each chunk."""
        off, total = 0, len(data)
        while off < total:
            ln = self._boundary(data, off, total - off)
            yield off, ln
            off += ln

    def digests(self, data: bytes):
        """Chunk digests, as REAPI SplitBlob would return them (SHA-256)."""
        out = []
        for off, ln in self.chunk(data):
            out.append((hashlib.sha256(data[off:off + ln]).hexdigest(), ln))
        return out


if __name__ == "__main__":
    import sys
    c = FastCdcChunker()
    for path in sys.argv[1:]:
        d = open(path, "rb").read()
        ch = c.digests(d)
        print(f"{path}: {len(d)} bytes -> {len(ch)} chunks "
              f"(avg {len(d) // max(1, len(ch))})")
