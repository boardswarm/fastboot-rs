# Lowlevel android sparse image parsing helpers

An android space image is a sparse representation of a potential output file. The format consist of a [FileHeader] followed by a [number](FileHeader::chunks) of [ChunkHeader]s and their associated data

| Sparse image    |
| ----------------|
| File header     |
| Chunk 0 header  |
| Chunk 0 data    |
| Chunk 1 header  |
| Chunk 1 data    |
| ....            |
| Chunk N header  |
| Chunk N data    |

The size of data in a chunk depends on the [ChunkType] and can be determined with [ChunkHeader::data_size]
