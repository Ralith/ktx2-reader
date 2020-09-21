//! Reading, validating and parsing of [`KTX v.2`] files.  
//! **Currently SUPER COMPRESSION is NOT supported.**
//!
//! [`KTX v.2`]: https://github.khronos.org/KTX-Specification/
pub mod error;
pub mod format;

use crate::format::Format;

use crate::error::{ParseError, ReadError, ReadToError};
use byteorder::{ByteOrder, NativeEndian};
use std::convert::TryInto;
use std::io::SeekFrom;
use tokio::io::AsyncSeek;
use tokio::prelude::*;

/// Struct to read [`KTX v.2`] files.  
///
/// [`KTX v.2`]: https://github.khronos.org/KTX-Specification/
pub struct Reader<T> {
    input: T,
    head: Header,
    levels_index: Vec<LevelIndex>,
}

/// Implementation of [Reader](struct.Reader.html) struct for async loading.
impl<T: AsyncRead + AsyncSeek + Unpin> Reader<T> {
    /// Create new instance of Reader.  
    /// Asyncroniosly reads and tries to parse data from `input`.
    /// # Errors
    /// If reading fails, returns [`ReadError::IoError`].  
    /// If parsing fails, returns [`ReadError::ParseError`].
    ///
    /// [`ReadError::IoError`]: error/enum.ReadError.html#variant.IoError
    /// [`ReadError::ParseError`]: error/enum.ReadError.html#variant.ParseError
    pub async fn new(mut input: T) -> ReadResult<Self> {
        let head = Self::read_head(&mut input).await?;
        let levels_index = Self::read_level_index(&mut input, &head).await?;
        Ok(Self {
            input,
            head,
            levels_index,
        })
    }

    /// Reads and tries to parse header of texture.  
    async fn read_head(input: &mut T) -> ReadResult<Header> {
        let mut head_bytes = [0; 48];
        input.read_exact(&mut head_bytes).await?;
        Self::test_identifier(&head_bytes)?;

        Ok(Header::from_bytes(&head_bytes)?)
    }

    /// Reads and tries to parse level index of texture.  
    ///
    /// [Level index](https://github.khronos.org/KTX-Specification/#_level_index) is a description of texture data layout.
    async fn read_level_index(input: &mut T, head: &Header) -> ReadResult<Vec<LevelIndex>> {
        const LEVEL_INDEX_START_BYTE: u64 = 80;
        const LEVEL_INDEX_BYTE_LEN: u32 = 24;
        let level_count = head.level_count.max(1);
        let level_index_bytes_len = level_count * LEVEL_INDEX_BYTE_LEN;
        let mut level_index_bytes: Vec<u8> = (0..level_index_bytes_len).map(|_| 0u8).collect();

        input.seek(SeekFrom::Start(LEVEL_INDEX_START_BYTE)).await?;
        input.read_exact(&mut level_index_bytes).await?;
        let mut infos = Vec::with_capacity(level_count as usize);
        for level_index in 0..level_count {
            let start_byte = (level_index * LEVEL_INDEX_BYTE_LEN) as usize;
            let end_byte = start_byte + LEVEL_INDEX_BYTE_LEN as usize;
            infos.push(LevelIndex::from_bytes(
                &level_index_bytes[start_byte..end_byte],
            ))
        }
        Ok(infos)
    }

    /// Reads data of texture.  
    /// Gets vector of bytes. It stores color data of texture.
    /// Layout of this data can be obtined from [`regions_description()`](#method.regions_description) method of self.
    pub async fn read_data(&mut self) -> ReadResult<Vec<u8>> {
        let data_len_bytes = self.data_len_bytes();
        let mut buffer = Vec::new();
        buffer.resize(data_len_bytes as usize, 0);
        self.read_data_to(&mut buffer)
            .await
            .map(|_| buffer)
            .map_err(|e| match e {
                ReadToError::ReadError(e) => e,
                ReadToError::BadBuffer(_) => {
                    panic!("Pass well sized buffer to read_data_to(), but got BadBuffer error")
                }
            })
    }

    /// ## Reads data of texture.
    /// Reads texture data to `buf`.
    /// Layout of this data can be obtined from [`regions_description()`](#method.regions_description) method of self.  
    /// Size of `buf` **MUST** be equal to expected data size. It can be obtained with [`data_len_bytes()`](#method.data_len_bytes) method.
    pub async fn read_data_to(&mut self, buf: &mut [u8]) -> ReadToResult<()> {
        let data_len_bytes = self.data_len_bytes();
        if buf.len() != data_len_bytes as usize {
            return Err(ReadToError::BadBuffer(data_len_bytes));
        }

        let data_start_byte = self.first_level_offset_bytes();
        self.input.seek(SeekFrom::Start(data_start_byte)).await?;

        self.input.read_exact(buf).await?;
        Ok(())
    }

    /// Tests first 12 bytes of input. If identifier is wrong,
    /// returns [`ReadError::ParseError`](error/enum.ReadError.html#variant.ParseError)
    /// with [`ParseError::BadIdentifier`](error/enum.ParseError.html#variant.BadIdentifier).
    fn test_identifier(head_bytes: &HeadBytes) -> ReadResult<()> {
        let mut red_id = [0; 12];
        red_id.copy_from_slice(&head_bytes[0..12]);
        if red_id == KTX2_IDENTIFIER {
            return Ok(());
        }
        Err(ReadError::ParseError(ParseError::BadIdentifier(red_id)))
    }

    /// Returns [`Header`](struct.Header.html) of texture.
    pub fn header(&self) -> &Header {
        &self.head
    }

    /// Returns vector of [`RegionDescription`](struct.RegionDescription.html) for texture.
    pub fn regions_description(&self) -> Vec<RegionDescription> {
        let base_offset = self.first_level_offset_bytes();
        self.levels_index
            .iter()
            .enumerate()
            .map(|(i, level)| self.region_from_level_index(i, level.offset - base_offset))
            .collect()
    }

    /// Start of texture data oofset in bytes.
    fn first_level_offset_bytes(&self) -> u64 {
        self.levels_index
            .iter()
            .map(|l| l.offset)
            .min()
            .expect("No levels got, but read some on constructing")
    }

    /// Last (by data offset) level in texture data.
    fn last_level(&self) -> LevelIndex {
        *self
            .levels_index
            .iter()
            .max_by_key(|l| l.offset)
            .expect("No levels got, but read some on constructing")
    }

    /// Full length of texture data.
    pub fn data_len_bytes(&self) -> u64 {
        let start_offset = self.first_level_offset_bytes();
        let last_level = self.last_level();
        last_level.offset + last_level.uncompressed_length_bytes - start_offset
    }

    /// Crates region info from level info.
    fn region_from_level_index(&self, i: usize, offset: u64) -> RegionDescription {
        RegionDescription {
            level: i as u32,
            layer_count: self.head.layer_count.max(1) * self.head.face_count,
            offset_bytes: offset,
            width: Self::level_size(self.head.base_width, i as u32),
            height: Self::level_size(self.head.base_height, i as u32),
            depth: Self::level_size(self.head.base_depth, i as u32),
        }
    }

    /// Size in pixels of `level`, with `base` size.
    fn level_size(base: u32, level: u32) -> u32 {
        (base >> level).max(1)
    }
}

/// Identifier, expected in start of input texture data.
static KTX2_IDENTIFIER: [u8; 12] = [
    0xAB, 0x4B, 0x54, 0x58, 0x20, 0x32, 0x30, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
];

/// Result of read data operation.
pub type ReadResult<T> = Result<T, ReadError>;

/// Result of reading data to buffer operation.
pub type ReadToResult<T> = Result<T, ReadToError>;

/// Result of parsing data operation.
pub type ParseResult<T> = Result<T, ParseError>;

/// Header of texture. Contains general information.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct Header {
    pub format: Format,
    pub type_size: u32,
    pub base_width: u32,
    pub base_height: u32,
    pub base_depth: u32,
    pub layer_count: u32,
    pub face_count: u32,
    pub level_count: u32,
    pub supercompression_scheme: u32,
}

impl Header {
    /// Crates Header from bytes array.
    pub fn from_bytes(data: &HeadBytes) -> ParseResult<Self> {
        let format_id = NativeEndian::read_u32(&data[12..16]);
        let format = format_id.try_into()?;

        Ok(Self {
            format,
            type_size: NativeEndian::read_u32(&data[16..20]),
            base_width: Self::parse_base_width(&data[20..24])?,
            base_height: NativeEndian::read_u32(&data[24..28]),
            base_depth: NativeEndian::read_u32(&data[28..32]),
            layer_count: NativeEndian::read_u32(&data[32..36]),
            face_count: Self::parse_face_count(&data[36..40])?,
            level_count: NativeEndian::read_u32(&data[40..44]),
            supercompression_scheme: Self::parse_supercompression_scheme(&data[44..48])?,
        })
    }

    fn parse_base_width(data: &[u8]) -> ParseResult<u32> {
        let result = NativeEndian::read_u32(&data[0..4]);
        match result {
            0 => Err(ParseError::ZeroWidth),
            _ => Ok(result),
        }
    }

    fn parse_face_count(data: &[u8]) -> ParseResult<u32> {
        let result = NativeEndian::read_u32(&data[0..4]);
        match result {
            0 => Err(ParseError::ZeroFaceCount),
            _ => Ok(result),
        }
    }

    fn parse_supercompression_scheme(data: &[u8]) -> ParseResult<u32> {
        let result = NativeEndian::read_u32(&data[0..4]);
        match result {
            0 => Ok(0),
            _ => Err(ParseError::UnsupportedFeature("supercompression scheme")),
        }
    }
}

/// Array, that stores data of start of texture.
type HeadBytes = [u8; 48];

/// Struct, that contains size and offset information about levels.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
struct LevelIndex {
    pub offset: u64,
    pub length_bytes: u64,
    pub uncompressed_length_bytes: u64,
}

impl LevelIndex {
    pub fn from_bytes(data: &[u8]) -> Self {
        Self {
            offset: NativeEndian::read_u64(&data[0..8]),
            length_bytes: NativeEndian::read_u64(&data[8..16]),
            uncompressed_length_bytes: NativeEndian::read_u64(&data[16..24]),
        }
    }
}

/// Describe texture regions e.g. mip-levels and layers.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct RegionDescription {
    pub level: u32,
    pub layer_count: u32,
    pub offset_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}
