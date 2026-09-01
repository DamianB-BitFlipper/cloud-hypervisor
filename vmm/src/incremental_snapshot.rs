// Copyright © 2026 Prime Intellect
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress, GuestAddressSpace};
use vm_migration::MigratableError;
use vm_migration::protocol::MemoryRangeTable;

use crate::GuestMemoryMmap;
use crate::migration::url_to_path;
use crate::vm_config::VmConfig;

pub const MEMORY_DELTA_FILE: &str = "memory.delta";
pub const MEMORY_MANIFEST_FILE: &str = "memory.manifest";
pub const COMPATIBILITY_FILE: &str = "compatibility.json";
pub const FORMAT_VERSION: u32 = 1;
pub const PAGE_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePhase {
    Capturing,
    Finalized,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemoryDeltaSegment {
    pub gpa: u64,
    pub length: u64,
    pub file_offset: u64,
    /// Offset where this segment must be written in a legacy `memory-ranges` file.
    pub restore_file_offset: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemoryRestoreRange {
    pub gpa: u64,
    pub length: u64,
    pub file_offset: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemoryDeltaManifest {
    pub format_version: u32,
    pub page_size: u64,
    pub capture_id: u64,
    pub restore_file_size: u64,
    pub restore_ranges: Vec<MemoryRestoreRange>,
    /// Apply segments in order. A later segment supersedes an earlier segment
    /// when pre-copy captured the same guest page more than once.
    pub segments: Vec<MemoryDeltaSegment>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CompatibilityDescriptor {
    pub format_version: u32,
    pub architecture: String,
    pub vmm_version: String,
    pub build_version: String,
    pub memory_size: u64,
    pub boot_vcpus: u32,
    pub max_vcpus: u32,
    pub page_size: u64,
}

impl CompatibilityDescriptor {
    pub fn new(vm_config: &VmConfig, vmm_version: &str, build_version: &str) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            architecture: std::env::consts::ARCH.to_string(),
            vmm_version: vmm_version.to_string(),
            build_version: build_version.to_string(),
            memory_size: vm_config.memory.total_size(),
            boot_vcpus: vm_config.cpus.boot_vcpus,
            max_vcpus: vm_config.cpus.max_vcpus,
            page_size: PAGE_SIZE,
        }
    }
}

pub struct IncrementalSnapshotCapture {
    id: u64,
    destination_url: String,
    memory_file: Option<File>,
    segments: Vec<MemoryDeltaSegment>,
    captured_ranges: MemoryRangeTable,
    bytes_written: u64,
    restore_file_size: u64,
    restore_ranges: Vec<MemoryRestoreRange>,
    phase: CapturePhase,
}

impl IncrementalSnapshotCapture {
    pub fn new(id: u64, destination_url: String) -> Result<Self, MigratableError> {
        let mut memory_path = url_to_path(&destination_url)?;
        memory_path.push(MEMORY_DELTA_FILE);

        let memory_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(memory_path)
            .map_err(|e| MigratableError::MigrateSend(e.into()))?;
        memory_file
            .set_permissions(fs::Permissions::from_mode(0o660))
            .map_err(|e| MigratableError::MigrateSend(e.into()))?;

        Ok(Self {
            id,
            destination_url,
            memory_file: Some(memory_file),
            segments: Vec::new(),
            captured_ranges: MemoryRangeTable::default(),
            bytes_written: 0,
            restore_file_size: 0,
            restore_ranges: Vec::new(),
            phase: CapturePhase::Capturing,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn phase(&self) -> CapturePhase {
        self.phase
    }

    pub fn destination_url(&self) -> &str {
        &self.destination_url
    }

    pub fn captured_ranges(&self) -> &MemoryRangeTable {
        &self.captured_ranges
    }

    pub fn mark_failed(&mut self) {
        self.phase = CapturePhase::Failed;
    }

    pub fn append_memory(
        &mut self,
        guest_memory: &vm_memory::GuestMemoryAtomic<GuestMemoryMmap>,
        ranges: MemoryRangeTable,
    ) -> Result<(), MigratableError> {
        if self.phase != CapturePhase::Capturing {
            return Err(MigratableError::MigrateSend(anyhow!(
                "capture {} is not accepting memory in phase {:?}",
                self.id,
                self.phase
            )));
        }

        // Record the entire cleared dirty set before copying. If copying fails,
        // abort can carry every address into the next generation.
        self.captured_ranges.extend(ranges.clone());

        let memory_file = self.memory_file.as_mut().ok_or_else(|| {
            MigratableError::MigrateSend(anyhow!("capture {} memory file is closed", self.id))
        })?;
        let memory = guest_memory.memory();

        for range in ranges.regions() {
            let file_offset = self.bytes_written;
            let mut range_offset = 0;
            while range_offset < range.length {
                let bytes_written = memory
                    .write_volatile_to(
                        GuestAddress(range.gpa + range_offset),
                        memory_file,
                        (range.length - range_offset) as usize,
                    )
                    .map_err(|e| {
                        MigratableError::MigrateSend(anyhow!(
                            "error writing guest memory for capture {}: {e}",
                            self.id
                        ))
                    })?;
                range_offset += bytes_written as u64;
                self.bytes_written += bytes_written as u64;
            }

            self.segments.push(MemoryDeltaSegment {
                gpa: range.gpa,
                length: range.length,
                file_offset,
                restore_file_offset: 0,
            });
        }

        Ok(())
    }

    pub fn write_compatibility(
        &self,
        descriptor: &CompatibilityDescriptor,
    ) -> Result<(), MigratableError> {
        write_json_file(&self.destination_url, COMPATIBILITY_FILE, descriptor)
    }

    pub fn set_restore_layout(&mut self, ranges: MemoryRangeTable) -> Result<(), MigratableError> {
        let mut file_offset = 0u64;
        self.restore_ranges.clear();
        for range in ranges.regions() {
            self.restore_ranges.push(MemoryRestoreRange {
                gpa: range.gpa,
                length: range.length,
                file_offset,
            });
            file_offset = file_offset.checked_add(range.length).ok_or_else(|| {
                MigratableError::MigrateSend(anyhow!("restore memory file size overflow"))
            })?;
        }
        self.restore_file_size = file_offset;

        for segment in &mut self.segments {
            let segment_end = segment.gpa.checked_add(segment.length).ok_or_else(|| {
                MigratableError::MigrateSend(anyhow!("memory delta GPA overflow"))
            })?;
            let restore_range = self
                .restore_ranges
                .iter()
                .find(|range| {
                    range
                        .gpa
                        .checked_add(range.length)
                        .is_some_and(|end| segment.gpa >= range.gpa && segment_end <= end)
                })
                .ok_or_else(|| {
                    MigratableError::MigrateSend(anyhow!(
                        "memory delta [{:#x}, {:#x}) is outside the restore layout",
                        segment.gpa,
                        segment_end
                    ))
                })?;
            segment.restore_file_offset = restore_range
                .file_offset
                .checked_add(segment.gpa - restore_range.gpa)
                .ok_or_else(|| {
                    MigratableError::MigrateSend(anyhow!("restore memory offset overflow"))
                })?;
        }
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<(), MigratableError> {
        if self.phase != CapturePhase::Capturing {
            return Err(MigratableError::MigrateSend(anyhow!(
                "capture {} cannot be finalized from phase {:?}",
                self.id,
                self.phase
            )));
        }

        if let Some(mut memory_file) = self.memory_file.take() {
            memory_file
                .flush()
                .map_err(|e| MigratableError::MigrateSend(e.into()))?;
        }

        let manifest = MemoryDeltaManifest {
            format_version: FORMAT_VERSION,
            page_size: PAGE_SIZE,
            capture_id: self.id,
            restore_file_size: self.restore_file_size,
            restore_ranges: self.restore_ranges.clone(),
            segments: self.segments.clone(),
        };
        write_json_file(&self.destination_url, MEMORY_MANIFEST_FILE, &manifest)?;

        self.phase = CapturePhase::Finalized;
        Ok(())
    }
}

fn write_json_file<T: Serialize>(
    destination_url: &str,
    filename: &str,
    value: &T,
) -> Result<(), MigratableError> {
    let mut path = url_to_path(destination_url)?;
    path.push(filename);

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| MigratableError::MigrateSend(e.into()))?;
    file.set_permissions(fs::Permissions::from_mode(0o660))
        .map_err(|e| MigratableError::MigrateSend(e.into()))?;
    serde_json::to_writer(&mut file, value).map_err(|e| MigratableError::MigrateSend(e.into()))?;
    file.flush()
        .map_err(|e| MigratableError::MigrateSend(e.into()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use vm_memory::{Bytes, GuestAddress, GuestMemoryAtomic};
    use vm_migration::protocol::{MemoryRange, MemoryRangeTable};
    use vmm_sys_util::tempdir::TempDir;

    use super::*;
    use crate::GuestMemoryMmap;

    #[test]
    fn ordered_segments_preserve_precopy_overwrites() {
        let temp_dir = TempDir::new().unwrap();
        let destination_url = format!("file://{}", temp_dir.as_path().display());
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        memory.write_slice(&[1; 0x1000], GuestAddress(0)).unwrap();
        let memory = GuestMemoryAtomic::new(memory);

        let mut capture = IncrementalSnapshotCapture::new(7, destination_url).unwrap();
        let mut first = MemoryRangeTable::default();
        first.push(MemoryRange {
            gpa: 0,
            length: 0x1000,
        });
        capture.append_memory(&memory, first).unwrap();

        memory
            .memory()
            .write_slice(&[2; 0x1000], GuestAddress(0))
            .unwrap();
        let mut second = MemoryRangeTable::default();
        second.push(MemoryRange {
            gpa: 0,
            length: 0x1000,
        });
        capture.append_memory(&memory, second).unwrap();
        let mut restore_layout = MemoryRangeTable::default();
        restore_layout.push(MemoryRange {
            gpa: 0,
            length: 0x4000,
        });
        capture.set_restore_layout(restore_layout).unwrap();
        capture.finalize().unwrap();

        let delta = fs::read(temp_dir.as_path().join(MEMORY_DELTA_FILE)).unwrap();
        assert_eq!(&delta[..0x1000], &[1; 0x1000]);
        assert_eq!(&delta[0x1000..], &[2; 0x1000]);

        let manifest: MemoryDeltaManifest = serde_json::from_slice(
            &fs::read(temp_dir.as_path().join(MEMORY_MANIFEST_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.capture_id, 7);
        assert_eq!(manifest.segments.len(), 2);
        assert_eq!(manifest.segments[0].file_offset, 0);
        assert_eq!(manifest.segments[1].file_offset, 0x1000);
        assert_eq!(manifest.restore_file_size, 0x4000);
        assert_eq!(manifest.segments[0].restore_file_offset, 0);
        assert_eq!(manifest.segments[1].restore_file_offset, 0);
    }
}
