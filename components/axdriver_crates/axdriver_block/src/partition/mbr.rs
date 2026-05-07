use alloc::{format, vec, vec::Vec};
use log::{debug, info};

use super::{PartitionInfo, PartitionRegion, PartitionTable, PartitionTableKind};
use crate::{BlockDriverOps, DevResult};

const MBR_SIGNATURE: &[u8; 2] = &[0x55, 0xAA];
const MBR_PARTITION_COUNT: usize = 4;
const MBR_BOOT_SIGNATURE: u8 = 0x80;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct MbrPartitionEntry {
    boot_status: u8,
    starting_chs: [u8; 3],
    partition_type: u8,
    ending_chs: [u8; 3],
    start_lba: u32,
    size_lba: u32,
}

#[repr(C, packed)]
#[derive(Debug)]
struct Mbr {
    bootstrap: [u8; 446],
    partitions: [MbrPartitionEntry; MBR_PARTITION_COUNT],
    signature: [u8; 2],
}

pub(super) fn scan_mbr_partitions<T: BlockDriverOps + ?Sized>(
    inner: &mut T,
) -> DevResult<Option<PartitionTable>> {
    let block_size = inner.block_size();
    if block_size < 512 {
        return Ok(None);
    }

    let mut block_buf = vec![0u8; block_size];
    inner.read_block(0, &mut block_buf).map_err(|_| {
        debug!("Failed to read MBR");
        crate::DevError::Io
    })?;

    // Check if we have enough data for MBR
    if block_buf.len() < core::mem::size_of::<Mbr>() {
        return Ok(None);
    }

    // Parse MBR structure
    let mbr: &Mbr = unsafe { &*(block_buf.as_ptr() as *const Mbr) };

    // Validate MBR signature
    if mbr.signature != *MBR_SIGNATURE {
        debug!("Invalid MBR signature: {:?}", mbr.signature);
        return Ok(None);
    }

    info!("Found valid MBR partition table");

    let mut partitions = Vec::new();

    for (i, entry) in mbr.partitions.iter().enumerate() {
        // Skip empty partitions (type 0 indicates empty)
        if entry.partition_type == 0 {
            continue;
        }

        let start_lba = entry.start_lba as u64;
        let end_lba = start_lba + entry.size_lba as u64;

        debug!(
            "MBR partition {}: type=0x{:02x}, lba={}..{}, size={} MB",
            i + 1,
            entry.partition_type,
            start_lba,
            end_lba,
            (entry.size_lba * 512 / (1024 * 1024))
        );

        partitions.push(PartitionInfo {
            index: i,
            table_kind: PartitionTableKind::Mbr,
            region: PartitionRegion {
                start_lba,
                end_lba,
            },
            name: Some(format!("mbr{}", i + 1)),
            part_uuid: None,
        });
    }

    if partitions.is_empty() {
        debug!("MBR found but no valid partitions");
        return Ok(None);
    }

    Ok(Some(PartitionTable {
        kind: PartitionTableKind::Mbr,
        partitions,
    }))
}
