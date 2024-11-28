use crate::{
    ChunkHeader, ChunkType, FileHeader, CHUNK_HEADER_BYTES_LEN, DEFAULT_BLOCKSIZE,
    FILE_HEADER_BYTES_LEN,
};
use thiserror::Error;

/// A definition of one chunk of a split image; When writing out or downloading to a device the
/// (chunk) header should be written out first followed by size bytes from the original file from
/// offset (in bytes) onwards
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitChunk {
    /// Chunk header
    pub header: ChunkHeader,
    /// Offset in the input file for the chunk data
    pub offset: usize,
    /// Amount of data to be copied from the input file (in bytes)
    pub size: usize,
}

/// A definition of a split sparse image; When writing out or downloading to a device the  (file)
/// header should be written first followed by each chunk
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Split {
    /// Global file header
    pub header: FileHeader,
    /// List of data chunks
    pub chunks: Vec<SplitChunk>,
}

impl Split {
    fn from_chunks(chunks: Vec<SplitChunk>, block_size: u32) -> Self {
        let n_chunks = chunks.len() as u32;
        let blocks = chunks.iter().map(|c| c.header.chunk_size).sum();

        let header = FileHeader {
            block_size,
            blocks,
            chunks: n_chunks,
            checksum: 0,
        };

        Split { header, chunks }
    }

    /// Total size of the sparse image that would be generated when writing out the split
    pub fn sparse_size(&self) -> usize {
        FILE_HEADER_BYTES_LEN
            + self
                .chunks
                .iter()
                .map(|c| c.header.total_size as usize)
                .sum::<usize>()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SplitBuilder {
    space: u32,
    block_size: u32,
    chunks: Vec<SplitChunk>,
}

impl SplitBuilder {
    fn new(block_size: u32, mut space: u32, blocks_offset: u32) -> Self {
        space -= FILE_HEADER_BYTES_LEN as u32;
        let chunks = if blocks_offset == 0 {
            vec![]
        } else {
            // Seek to the offset first
            let header = ChunkHeader::new_dontcare(blocks_offset);
            space -= header.total_size;
            vec![SplitChunk {
                header,
                offset: 0,
                size: 0,
            }]
        };
        Self {
            space,
            block_size,
            chunks,
        }
    }

    fn try_add_chunk(&mut self, chunk: &ChunkHeader, image_offset: usize) -> bool {
        if self.space > chunk.total_size {
            let split = SplitChunk {
                header: chunk.clone(),
                offset: image_offset,
                size: chunk.data_size(),
            };
            self.chunks.push(split);
            self.space -= chunk.total_size;
            true
        } else {
            false
        }
    }

    /// Add as much raw data as possible, returning the blocks taken up)
    fn add_raw(&mut self, image_offset: usize, blocks: u32) -> u32 {
        let left = self.space.saturating_sub(CHUNK_HEADER_BYTES_LEN as u32);
        let blocks_left = left / self.block_size;

        if blocks_left > 0 {
            let blocks = blocks.min(blocks_left);
            let header = ChunkHeader::new_raw(blocks, self.block_size);
            self.space -= header.total_size;

            self.chunks.push(SplitChunk {
                size: header.data_size(),
                offset: image_offset,
                header,
            });

            blocks
        } else {
            0
        }
    }

    fn finish(self) -> Split {
        Split::from_chunks(self.chunks, self.block_size)
    }
}

#[derive(Debug, Error)]
pub enum SplitError {
    #[error("Size is too small to fit chunks")]
    TooSmall,
}

fn check_minimal_size(size: u32, block_size: u32) -> Result<(), SplitError> {
    // At the very list the size we split into should be enough to have:
    // * A file header
    // * A Chunk header for an initial don't care block
    // * A Chunk header for a raw block and a single block
    if size < FILE_HEADER_BYTES_LEN as u32 + 2 * CHUNK_HEADER_BYTES_LEN as u32 + block_size {
        return Err(SplitError::TooSmall);
    }
    Ok(())
}

/// Split an existing sparse image based on its file header and chunks into multiple splits fitting
/// into the given `size`
pub fn split_image(
    header: &FileHeader,
    chunks: &[ChunkHeader],
    size: u32,
) -> Result<Vec<Split>, SplitError> {
    check_minimal_size(size, header.block_size)?;
    let (_, _, builder, mut splits) = chunks.iter().try_fold(
        (
            // output offset in blocks
            0,
            // Start of the first data area (after initial file and chunk header
            FILE_HEADER_BYTES_LEN + CHUNK_HEADER_BYTES_LEN,
            SplitBuilder::new(header.block_size, size, 0),
            // Splits collector
            vec![],
        ),
        |(block_offset, image_offset, mut builder, mut splits), chunk| {
            if !builder.try_add_chunk(chunk, image_offset) {
                if chunk.chunk_type == ChunkType::Raw {
                    // Try packing in partial chunks
                    let mut blocks = 0;
                    loop {
                        blocks += builder.add_raw(
                            image_offset + (blocks * header.block_size) as usize,
                            chunk.chunk_size - blocks,
                        );

                        if blocks >= chunk.chunk_size {
                            break;
                        } else {
                            splits.push(builder.finish());
                            builder =
                                SplitBuilder::new(header.block_size, size, block_offset + blocks);
                        }
                    }
                } else {
                    splits.push(builder.finish());
                    builder = SplitBuilder::new(header.block_size, size, block_offset);
                    if !builder.try_add_chunk(chunk, image_offset) {
                        return Err(SplitError::TooSmall);
                    }
                }
            }
            Ok((
                block_offset + chunk.chunk_size,
                image_offset + chunk.total_size as usize,
                builder,
                splits,
            ))
        },
    )?;
    splits.push(builder.finish());
    Ok(splits)
}

/// Generate a set of splits for a raw image of a given `raw_size` each fitting within `size`; The
/// raw size is rounded up to multiple of [DEFAULT_BLOCKSIZE] as that's the minimal granularity.
/// When writing out the android sparse image the data should just be padded as needed as well!
pub fn split_raw(raw_size: usize, size: u32) -> Result<Vec<Split>, SplitError> {
    check_minimal_size(size, DEFAULT_BLOCKSIZE)?;
    let raw_blocks = raw_size.div_ceil(DEFAULT_BLOCKSIZE as usize) as u32;

    let mut block_offset = 0;
    let mut splits = vec![];

    while raw_blocks > block_offset {
        let mut builder = SplitBuilder::new(DEFAULT_BLOCKSIZE, size, block_offset);
        block_offset += builder.add_raw(
            (block_offset * DEFAULT_BLOCKSIZE) as usize,
            raw_blocks - block_offset,
        );
        splits.push(builder.finish());
    }
    Ok(splits)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn split_simple() {
        let header = FileHeader {
            block_size: 4096,
            blocks: 1024,
            chunks: 2,
            checksum: 0,
        };
        let chunks = [
            ChunkHeader::new_fill(8),
            ChunkHeader::new_raw(1024 - 8, 4096),
        ];

        let split = split_image(&header, &chunks, 1024 * 4096).unwrap();
        assert_eq!(split.len(), 1);
        let split = &split[0];

        assert_eq!(split.header, header);
        assert_eq!(split.chunks.len(), 2);
        assert_eq!(
            &split.chunks[0],
            &SplitChunk {
                header: chunks[0].clone(),
                offset: FILE_HEADER_BYTES_LEN + CHUNK_HEADER_BYTES_LEN,
                size: chunks[0].data_size()
            }
        );
        assert_eq!(
            &split.chunks[1],
            &SplitChunk {
                header: chunks[1].clone(),
                offset: FILE_HEADER_BYTES_LEN + 2 * CHUNK_HEADER_BYTES_LEN + 4,
                size: chunks[1].data_size(),
            }
        );
    }

    #[test]
    fn split_multiple() {
        let header = FileHeader {
            block_size: 4096,
            blocks: 2048,
            chunks: 2,
            checksum: 0,
        };
        let chunks = [
            ChunkHeader::new_fill(8),
            ChunkHeader::new_raw(1024 - 8, 4096),
            ChunkHeader::new_raw(1024 - 8, 4096),
            ChunkHeader::new_fill(8),
        ];
        let expected = [
            Split {
                header: FileHeader {
                    block_size: 4096,
                    blocks: 519,
                    chunks: 2,
                    checksum: 0,
                },
                chunks: vec![
                    SplitChunk {
                        header: ChunkHeader::new_fill(8),
                        offset: FILE_HEADER_BYTES_LEN + CHUNK_HEADER_BYTES_LEN,
                        size: 4,
                    },
                    SplitChunk {
                        header: ChunkHeader::new_raw(511, 4096),
                        offset: FILE_HEADER_BYTES_LEN + 2 * CHUNK_HEADER_BYTES_LEN + 4,
                        size: 511 * 4096,
                    },
                ],
            },
            Split {
                header: FileHeader {
                    block_size: 4096,
                    blocks: 519 + 511,
                    chunks: 3,
                    checksum: 0,
                },
                chunks: vec![
                    SplitChunk {
                        header: ChunkHeader::new_dontcare(519),
                        offset: 0,
                        size: 0,
                    },
                    // Finalizing first raw block, 1024 - 519 left: 505
                    SplitChunk {
                        header: ChunkHeader::new_raw(505, 4096),
                        offset: FILE_HEADER_BYTES_LEN + 2 * CHUNK_HEADER_BYTES_LEN + 4 + 511 * 4096,
                        size: 505 * 4096,
                    },
                    // First part of the second raw chunk, 511 - 505 left: 6
                    SplitChunk {
                        header: ChunkHeader::new_raw(6, 4096),
                        offset: FILE_HEADER_BYTES_LEN
                            + 3 * CHUNK_HEADER_BYTES_LEN
                            + 4
                            + 1016 * 4096,
                        size: 6 * 4096,
                    },
                ],
            },
            Split {
                header: FileHeader {
                    block_size: 4096,
                    blocks: 519 + 511 + 511,
                    chunks: 2,
                    checksum: 0,
                },
                chunks: vec![
                    SplitChunk {
                        header: ChunkHeader::new_dontcare(519 + 511),
                        offset: 0,
                        size: 0,
                    },
                    // Second part of the second raw chunk, 6 were in the last chunk
                    SplitChunk {
                        header: ChunkHeader::new_raw(511, 4096),
                        offset: FILE_HEADER_BYTES_LEN
                            + 3 * CHUNK_HEADER_BYTES_LEN
                            + 4
                            + 1016 * 4096
                            + 6 * 4096,
                        size: 511 * 4096,
                    },
                ],
            },
            Split {
                header: FileHeader {
                    block_size: 4096,
                    blocks: 2048,
                    chunks: 3,
                    checksum: 0,
                },
                chunks: vec![
                    SplitChunk {
                        header: ChunkHeader::new_dontcare(519 + 511 + 511),
                        offset: 0,
                        size: 0,
                    },
                    // Final part of the second raw chunk, 6 + 511 already accounted for, so 499
                    // left of 1016
                    SplitChunk {
                        header: ChunkHeader::new_raw(499, 4096),
                        offset: FILE_HEADER_BYTES_LEN
                            + 3 * CHUNK_HEADER_BYTES_LEN
                            + 4
                            + 1016 * 4096
                            + 517 * 4096,
                        size: 499 * 4096,
                    },
                    // Second fill
                    SplitChunk {
                        header: ChunkHeader::new_fill(8),
                        offset: FILE_HEADER_BYTES_LEN
                            + 4 * CHUNK_HEADER_BYTES_LEN
                            + 4
                            + 1016 * 4096
                            + 1016 * 4096,
                        size: 4,
                    },
                ],
            },
        ];

        let splits = split_image(&header, &chunks, 512 * 4096).unwrap();
        for (i, (split, expected)) in splits.iter().zip(expected.iter()).enumerate() {
            assert_eq!(split, expected, "split {i} mismatch");
        }
        assert_eq!(splits.len(), expected.len());
    }

    #[test]
    fn test_split_raw() {
        let splits = split_raw(8 * DEFAULT_BLOCKSIZE as usize, 3 * DEFAULT_BLOCKSIZE).unwrap();
        assert_eq!(splits.len(), 4, "Incorrect parts: {splits:?}");
        for (i, split) in splits.iter().enumerate() {
            assert_eq!(split.header.block_size, 4096);
            assert_eq!(split.header.checksum, 0);
            let raw = if i == 0 {
                assert_eq!(split.header.chunks, 1);
                assert_eq!(split.chunks.len(), 1);
                &split.chunks[0]
            } else {
                assert_eq!(split.header.chunks, 2);
                assert_eq!(split.chunks.len(), 2);
                assert_eq!(
                    split.chunks[0],
                    SplitChunk {
                        header: ChunkHeader {
                            chunk_type: ChunkType::DontCare,
                            chunk_size: 2 * i as u32,
                            total_size: CHUNK_HEADER_BYTES_LEN as u32
                        },
                        offset: 0,
                        size: 0
                    },
                    "chunk {i}"
                );
                &split.chunks[1]
            };
            assert_eq!(
                raw,
                &SplitChunk {
                    header: ChunkHeader {
                        chunk_type: ChunkType::Raw,
                        chunk_size: 2,
                        total_size: 2 * DEFAULT_BLOCKSIZE + CHUNK_HEADER_BYTES_LEN as u32
                    },
                    offset: 2 * i * DEFAULT_BLOCKSIZE as usize,
                    size: 2 * DEFAULT_BLOCKSIZE as usize
                },
                "chunk {i}"
            );
        }
    }
}
