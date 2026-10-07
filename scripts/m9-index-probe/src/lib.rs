// SPDX-License-Identifier: BSD-2-Clause
//! Probe for ADR 034's open question 7.
//!
//! The question: a message is 128 words, the first three of which travel
//! in registers. For the rest, is the buffer indexed `buffer[i]` --
//! wasting its first three slots so the index needs no adjustment -- or
//! `buffer[i - 3]`, which uses all 128 but subtracts on every access?
//!
//! ADR 034 originally chose the first **without measuring**, while
//! ADR 040 was telling everyone else not to optimise without measuring.
//! This is that measurement. Not part of the kernel build; run by
//! `scripts/m9-index-probe/probe.sh` and quoted in ADR 034.

#![no_std]

pub const WORDS: usize = 128;
pub const REG_WORDS: usize = 3;

/// Variant A: `buffer[i]`, first three slots unused. 125 words usable.
#[inline(never)]
pub fn read_wasteful(buffer: &[u64; WORDS], regs: &[u64; REG_WORDS], i: usize) -> u64 {
    if i < REG_WORDS {
        regs[i]
    } else {
        buffer[i]
    }
}

/// Variant B: `buffer[i - 3]`, all 128 words usable.
#[inline(never)]
pub fn read_offset(buffer: &[u64; WORDS], regs: &[u64; REG_WORDS], i: usize) -> u64 {
    if i < REG_WORDS {
        regs[i]
    } else {
        buffer[i - REG_WORDS]
    }
}

/// The loop that actually matters: copying a message between two TCBs.
#[inline(never)]
pub fn copy_wasteful(src: &[u64; WORDS], dst: &mut [u64; WORDS], length: usize) {
    for i in REG_WORDS..length.min(WORDS) {
        dst[i] = src[i];
    }
}

#[inline(never)]
pub fn copy_offset(src: &[u64; WORDS], dst: &mut [u64; WORDS], length: usize) {
    for i in REG_WORDS..length.min(WORDS) {
        dst[i - REG_WORDS] = src[i - REG_WORDS];
    }
}