// Copyright 2026 The Cloud Hypervisor Authors. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

//! Sparse-operation helpers for regular files and block devices.

use std::io;
use std::os::unix::io::RawFd;

use libc::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FALLOC_FL_ZERO_RANGE};

// Linux UAPI: `_IO(0x12, 119)`, argument is `__u64 range[2]`.
pub const BLKDISCARD: libc::c_ulong = 0x1277;
// Linux UAPI: `_IO(0x12, 127)`, argument is `__u64 range[2]`.
pub const BLKZEROOUT: libc::c_ulong = 0x127f;

fn blk_range_ioctl(fd: RawFd, request: libc::c_ulong, offset: u64, length: u64) -> io::Result<()> {
    let range: [u64; 2] = [offset, length];

    // SAFETY: `fd` is owned by the caller and `range` has the `__u64[2]`
    // layout required by BLKDISCARD and BLKZEROOUT.
    let result = unsafe { libc::ioctl(fd, request as _, &range) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn blkdiscard(fd: RawFd, offset: u64, length: u64) -> io::Result<()> {
    blk_range_ioctl(fd, BLKDISCARD, offset, length)
}

pub(crate) fn blkzeroout(fd: RawFd, offset: u64, length: u64) -> io::Result<()> {
    blk_range_ioctl(fd, BLKZEROOUT, offset, length)
}

pub(crate) fn punch_hole(
    fd: RawFd,
    is_block_device: bool,
    offset: u64,
    length: u64,
) -> io::Result<()> {
    if is_block_device {
        blkdiscard(fd, offset, length)
    } else {
        fallocate(
            fd,
            FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
            offset,
            length,
        )
    }
}

pub(crate) fn write_zeroes(
    fd: RawFd,
    is_block_device: bool,
    offset: u64,
    length: u64,
) -> io::Result<()> {
    if is_block_device {
        blkzeroout(fd, offset, length)
    } else {
        fallocate(
            fd,
            FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE,
            offset,
            length,
        )
    }
}

fn fallocate(fd: RawFd, mode: libc::c_int, offset: u64, length: u64) -> io::Result<()> {
    // SAFETY: FFI call with a valid fd; fallocate does not access userspace
    // memory through a supplied pointer.
    let result = unsafe { libc::fallocate(fd, mode, offset as libc::off_t, length as libc::off_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
