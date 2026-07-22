//! FastCDC 2020 content-defined chunking compatible with Bazel's
//! `FastCdcChunker` defaults and two-byte loop.

use std::sync::LazyLock;

use anyhow::{ensure, Result};
use serde::Deserialize;

pub const DEFAULT_AVG: usize = 512 * 1024;
pub const DEFAULT_MIN: usize = DEFAULT_AVG / 4;
pub const DEFAULT_MAX: usize = DEFAULT_AVG * 4;
pub const DEFAULT_NORMALIZATION: usize = 2;
pub const DEFAULT_SEED: u64 = 0;

#[derive(Deserialize)]
struct Parameters {
    masks: Vec<u64>,
    gear: Vec<u64>,
}

// These constants are vendored from Bazel's Java implementation (the research
// harness retains the extraction source). Embedding them keeps packaged crates
// self-contained; parsing happens once per process.
static PARAMETERS: LazyLock<Parameters> = LazyLock::new(|| {
    serde_json::from_str(include_str!("fastcdc_params.json"))
        .expect("checked-in FastCDC parameters must be valid")
});

#[derive(Debug, Clone)]
pub struct FastCdc {
    pub average: usize,
    pub minimum: usize,
    pub maximum: usize,
    mask_small: u64,
    mask_large: u64,
    seed: u64,
}

impl Default for FastCdc {
    fn default() -> Self {
        Self::new(
            DEFAULT_AVG,
            DEFAULT_MIN,
            DEFAULT_MAX,
            DEFAULT_NORMALIZATION,
            DEFAULT_SEED,
        )
        .expect("Bazel FastCDC defaults are valid")
    }
}

impl FastCdc {
    pub fn new(
        average: usize,
        minimum: usize,
        maximum: usize,
        normalization: usize,
        seed: u64,
    ) -> Result<Self> {
        ensure!(
            average.is_power_of_two(),
            "average chunk size must be a power of two"
        );
        ensure!(minimum > 0, "minimum chunk size must be positive");
        ensure!(minimum <= average, "minimum chunk size exceeds average");
        ensure!(average <= maximum, "average chunk size exceeds maximum");
        let bits = average.trailing_zeros() as usize;
        ensure!(bits + normalization <= 25, "normalization is too large");
        ensure!(bits >= normalization + 5, "normalization is too large");
        ensure!(
            PARAMETERS.gear.len() == 256,
            "FastCDC gear table must have 256 entries"
        );
        Ok(Self {
            average,
            minimum,
            maximum,
            mask_small: PARAMETERS.masks[bits + normalization],
            mask_large: PARAMETERS.masks[bits - normalization],
            seed,
        })
    }

    /// Return the length of the first chunk in `bytes`.
    pub fn boundary(&self, bytes: &[u8]) -> usize {
        if bytes.len() <= self.minimum {
            return bytes.len();
        }
        let end = bytes.len().min(self.maximum);
        let center = end.min(self.average);
        let minimum = self.minimum & !1;
        let center = center & !1;
        let remaining = end & !1;
        let seed_shifted = self.seed << 1;
        let small_shifted = self.mask_small << 1;
        let large_shifted = self.mask_large << 1;
        let mut hash = 0u64;

        for offset in (minimum..center).step_by(2) {
            hash = hash
                .wrapping_shl(2)
                .wrapping_add((PARAMETERS.gear[bytes[offset] as usize] << 1) ^ seed_shifted);
            if hash & small_shifted == 0 {
                return offset;
            }
            hash = hash.wrapping_add(PARAMETERS.gear[bytes[offset + 1] as usize] ^ self.seed);
            if hash & self.mask_small == 0 {
                return offset + 1;
            }
        }
        for offset in (center..remaining).step_by(2) {
            hash = hash
                .wrapping_shl(2)
                .wrapping_add((PARAMETERS.gear[bytes[offset] as usize] << 1) ^ seed_shifted);
            if hash & large_shifted == 0 {
                return offset;
            }
            hash = hash.wrapping_add(PARAMETERS.gear[bytes[offset + 1] as usize] ^ self.seed);
            if hash & self.mask_large == 0 {
                return offset + 1;
            }
        }
        end
    }

    pub fn chunks<'a>(&'a self, bytes: &'a [u8]) -> Chunks<'a> {
        Chunks {
            chunker: self,
            bytes,
            offset: 0,
        }
    }
}

pub struct Chunks<'a> {
    chunker: &'a FastCdc,
    bytes: &'a [u8],
    offset: usize,
}

impl Iterator for Chunks<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.bytes.len() {
            return None;
        }
        let length = self.chunker.boundary(&self.bytes[self.offset..]);
        let offset = self.offset;
        self.offset += length;
        Some((offset, length))
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn bazel_reference_vector_matches_every_boundary() {
        let mut bytes = Vec::with_capacity(5 * 1024 * 1024);
        for index in 0u64..(5 * 1024 * 1024 / 32) as u64 {
            bytes.extend_from_slice(&Sha256::digest(index.to_le_bytes()));
        }
        let lengths = FastCdc::default()
            .chunks(&bytes)
            .map(|(_, length)| length)
            .collect::<Vec<_>>();
        assert_eq!(
            lengths,
            [
                560_905, 636_753, 708_026, 322_071, 533_378, 662_298, 556_544, 573_335, 617_449,
                72_121,
            ]
        );
        assert_eq!(lengths.iter().sum::<usize>(), bytes.len());
    }

    #[test]
    fn small_payload_is_one_chunk() {
        let bytes = vec![0; DEFAULT_MIN - 1];
        assert_eq!(
            FastCdc::default().chunks(&bytes).collect::<Vec<_>>(),
            [(0, bytes.len())]
        );
    }
}
