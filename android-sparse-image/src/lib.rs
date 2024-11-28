#![doc = include_str!("../README.md")]

/// Helpers to split an image into multiple smaller ones
pub mod split;

use bytes::{Buf, BufMut};
use log::trace;
use strum::FromRepr;
use thiserror::Error;

/// Length of the file header in bytes
pub const FILE_HEADER_BYTES_LEN: usize = 28;
/// Length of the chunk header in bytes
pub const CHUNK_HEADER_BYTES_LEN: usize = 12;
/// File magic - This are the first 4 bytes in little-endian
pub const HEADER_MAGIC: u32 = 0xed26ff3a;
pub const DEFAULT_BLOCKSIZE: u32 = 4096;

/// Byte parsing errors
#[derive(Clone, Debug, Error)]
pub enum ParseError {
    #[error("Header has an unknown magic value")]
    UnknownMagic,
    #[error("Header has an unknown version")]
    UnknownVersion,
    #[error("Header has an unexpected header or chunk size")]
    UnexpectedSize,
    #[error("Header has an unknown chunk type")]
    UnknownChunkType,
}

/// Byte array which fits a file header
pub type FileHeaderBytes = [u8; FILE_HEADER_BYTES_LEN];
/// Global file header
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    /// Block size in bytes (should be multiple of 4)
    pub block_size: u32,
    /// Number of blocks in the expanded image
    pub blocks: u32,
    /// Number of chunks in the sparse image
    pub chunks: u32,
    /// Optional CRC32 Checksum
    pub checksum: u32,
}

impl FileHeader {
    /// Create new FileHeader from a raw header
    pub fn from_bytes(bytes: &FileHeaderBytes) -> Result<FileHeader, ParseError> {
        let mut bytes = &bytes[..];

        let magic = bytes.get_u32_le();
        if magic != HEADER_MAGIC {
            trace!("Unrecognized header magic: {:x}", magic);
            return Err(ParseError::UnknownMagic);
        }

        let major = bytes.get_u16_le();
        if major != 0x1 {
            trace!("Unrecognized major versions: {:x}", major);
            return Err(ParseError::UnknownVersion);
        }

        let minor = bytes.get_u16_le();
        if minor != 0x0 {
            trace!("Unrecognized minor versions: {:x}", minor);
            return Err(ParseError::UnknownVersion);
        }

        let header_len = bytes.get_u16_le();
        if FILE_HEADER_BYTES_LEN != header_len.into() {
            trace!("Unexpected header size: {}", header_len);
            return Err(ParseError::UnexpectedSize);
        }

        let chunk_header_len = bytes.get_u16_le();
        if CHUNK_HEADER_BYTES_LEN != chunk_header_len.into() {
            trace!("Unexpected chunk header size: {}", chunk_header_len);
            return Err(ParseError::UnexpectedSize);
        }

        let block_size = bytes.get_u32_le();
        let blocks = bytes.get_u32_le();
        let chunks = bytes.get_u32_le();
        let checksum = bytes.get_u32_le();

        Ok(FileHeader {
            block_size,
            blocks,
            chunks,
            checksum,
        })
    }

    /// Convert into a raw header
    pub fn to_bytes(&self) -> FileHeaderBytes {
        let mut bytes = [0; FILE_HEADER_BYTES_LEN];
        let mut w = &mut bytes[..];
        w.put_u32_le(HEADER_MAGIC);
        // Version 1.0
        w.put_u16_le(0x1);
        w.put_u16_le(0x0);
        w.put_u16_le(FILE_HEADER_BYTES_LEN as u16);
        w.put_u16_le(CHUNK_HEADER_BYTES_LEN as u16);
        w.put_u32_le(self.block_size);
        w.put_u32_le(self.blocks);
        w.put_u32_le(self.chunks);
        w.put_u32_le(self.checksum);

        bytes
    }

    pub fn total_size(&self) -> usize {
        self.blocks as usize * self.block_size as usize
    }
}

/// Type of a chunk
#[derive(Copy, Clone, Debug, FromRepr, Eq, PartialEq)]
pub enum ChunkType {
    /// Chunk header is followed by raw content for [ChunkHeader::out_size] bytes; Should be copied
    /// to the output
    Raw = 0xcac1,
    /// Chunk header is followed by 4 bytes; which should be used to fill the output
    Fill = 0xcac2,
    /// No data after the chunk; The next [ChunkHeader::out_size] bytes can be filled with any
    /// content
    DontCare = 0xcac3,
    /// Chunk header is followed by 4 bytes, which is a crc32 checksum
    Crc32 = 0xcac4,
}

/// Byte array which fits a chunk header
pub type ChunkHeaderBytes = [u8; CHUNK_HEADER_BYTES_LEN];

/// Header of a chunk
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkHeader {
    /// The type of the chunk
    pub chunk_type: ChunkType,
    /// Output size of the chunk in blocksize
    pub chunk_size: u32,
    /// Size of the chunk in the sparse image
    pub total_size: u32,
}

impl ChunkHeader {
    /// Create a don't care header for a given length in blocks
    pub fn new_dontcare(blocks: u32) -> Self {
        ChunkHeader {
            chunk_type: ChunkType::DontCare,
            total_size: CHUNK_HEADER_BYTES_LEN as u32,
            chunk_size: blocks,
        }
    }

    /// Create a new raw header for a given amount in blocks for block_size
    ///
    /// The actual data should follow this header
    pub fn new_raw(blocks: u32, block_size: u32) -> Self {
        ChunkHeader {
            chunk_type: ChunkType::Raw,
            chunk_size: blocks,
            total_size: (CHUNK_HEADER_BYTES_LEN as u32)
                .saturating_add(blocks.saturating_mul(block_size)),
        }
    }

    /// Create a new fill header for a given amount of blocks to be filled
    ///
    /// The header should be followed by 4 bytes indicate the data to fill with
    pub fn new_fill(blocks: u32) -> Self {
        ChunkHeader {
            chunk_type: ChunkType::Fill,
            chunk_size: blocks,
            total_size: CHUNK_HEADER_BYTES_LEN as u32 + 4,
        }
    }

    /// Create new ChunkHeader from a raw header
    pub fn from_bytes(bytes: &ChunkHeaderBytes) -> Result<ChunkHeader, ParseError> {
        let mut bytes = &bytes[..];
        let chunk_type = bytes.get_u16_le();
        let Some(chunk_type) = ChunkType::from_repr(chunk_type.into()) else {
            trace!("Unknown chunk type: {}", chunk_type);
            return Err(ParseError::UnknownChunkType);
        };
        // reserved
        bytes.advance(2);
        let chunk_size = bytes.get_u32_le();
        let total_size = bytes.get_u32_le();

        Ok(ChunkHeader {
            chunk_type,
            chunk_size,
            total_size,
        })
    }

    /// Convert into a raw header
    pub fn to_bytes(&self) -> ChunkHeaderBytes {
        let mut bytes = [0; CHUNK_HEADER_BYTES_LEN];
        let mut w = &mut bytes[..];
        w.put_u16_le(self.chunk_type as u16);
        w.put_u16_le(0x0);
        w.put_u32_le(self.chunk_size);
        w.put_u32_le(self.total_size);
        bytes
    }

    /// Resulting size of this chunk in the output
    pub fn out_size(&self, header: &FileHeader) -> usize {
        self.chunk_size as usize * header.block_size as usize
    }

    /// Data bytes after the header
    pub fn data_size(&self) -> usize {
        (self.total_size as usize).saturating_sub(CHUNK_HEADER_BYTES_LEN)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn file_header_parse() {
        let data = [
            0x3au8, 0xff, 0x26, 0xed, 0x01, 0x00, 0x00, 0x00, 0x1c, 0x00, 0x0c, 0x00, 0x00, 0x10,
            0x00, 0x00, 0x77, 0x39, 0x14, 0x00, 0xb1, 0x00, 0x00, 0x00, 0xaa, 0x00, 0x00, 0xcc,
        ];

        let h = FileHeader::from_bytes(&data).unwrap();
        assert_eq!(
            h,
            FileHeader {
                block_size: 4096,
                blocks: 1325431,
                chunks: 177,
                checksum: 0xcc0000aa,
            }
        );
    }

    #[test]
    fn file_header_roundtrip() {
        let orig = FileHeader {
            block_size: 4096,
            blocks: 1024,
            chunks: 42,
            checksum: 0xabcd,
        };

        let b = orig.to_bytes();
        let echo = FileHeader::from_bytes(&b).unwrap();

        assert_eq!(orig, echo);
    }

    #[test]
    fn chunk_header_parse() {
        let data = [
            0xc3u8, 0xca, 0x0, 0x0, 0x1f, 0xf1, 0xaa, 0xbb, 0x0c, 0x00, 0x00, 0x00,
        ];

        let h = ChunkHeader::from_bytes(&data).unwrap();
        assert_eq!(
            h,
            ChunkHeader {
                chunk_type: ChunkType::DontCare,
                chunk_size: 0xbbaaf11f,
                total_size: CHUNK_HEADER_BYTES_LEN as u32,
            }
        );
    }

    #[test]
    fn chunk_header_roundtrip() {
        let orig = ChunkHeader {
            chunk_type: ChunkType::Fill,
            chunk_size: 8,
            total_size: (CHUNK_HEADER_BYTES_LEN + 4) as u32,
        };

        let b = orig.to_bytes();
        let echo = ChunkHeader::from_bytes(&b).unwrap();

        assert_eq!(orig, echo);
    }
}
