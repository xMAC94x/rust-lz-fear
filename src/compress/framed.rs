use byteorder::{LE, WriteBytesExt};
use std::hash::Hasher;
use std::io::{self, Read, Write, Seek, SeekFrom, ErrorKind};
use std::mem;
use twox_hash::XxHash32;
use thiserror::Error;
use fehler::{throw, throws};

use crate::{MAGIC, INCOMPRESSIBLE, WINDOW_SIZE};
use crate::header::{Flags, BlockDescriptor};
use super::raw::{U32Table, compress2, EncoderTable};


pub struct CompressionSettings<'a> {
    independent_blocks: bool,
    block_checksums: bool,
    content_checksum: bool,
    block_size: usize,
    dictionary: Option<&'a [u8]>,
    dictionary_id: Option<u32>,
}
impl<'a> Default for CompressionSettings<'a> {
    fn default() -> Self {
        Self {
            independent_blocks: true,
            block_checksums: false,
            content_checksum: true,
            block_size: 4 * 1024 * 1024,
            dictionary: None,
            dictionary_id: None,
        }
    }
}
impl<'a> CompressionSettings<'a> {
    pub fn independent_blocks(&mut self, v: bool) -> &mut Self {
        self.independent_blocks = v;
        self
    }
    pub fn block_checksums(&mut self, v: bool) -> &mut Self {
        self.block_checksums = v;
        self
    }
    pub fn content_checksum(&mut self, v: bool) -> &mut Self {
        self.content_checksum = v;
        self
    }
    /// Only valid values are 4MB, 1MB, 256KB, 64KB
    /// (TODO: better interface for this)
    pub fn block_size(&mut self, v: usize) -> &mut Self {
        self.block_size = v;
        self
    }
    pub fn dictionary(&mut self, id: u32, dict: &'a [u8]) -> &mut Self {
        self.dictionary_id = Some(id);
        self.dictionary = Some(dict);
        self
    }

    /// The dictionary id header field is quite obviously intended to tell anyone trying to decompress your frame which dictionary to use.
    /// So it is only natural to assume that the *absence* of a dictionary id indicates that no dictionary was used.
    ///
    /// Unfortunately this assumption turns out to be incorrect. The LZ4 CLI simply never writes a dictionary id.
    /// The major downside is that you can no longer distinguish corrupted data from a missing dictionary
    /// (unless you write block checksums, which the LZ4 CLI also never does).
    ///
    /// Hence, this library is opinionated in the sense that we always want you to specify either neither or both of these things
    /// (the LZ4 CLI basically just ignores the dictionary id completely and only cares about whether you specify a dictionary parameter or not).
    ///
    /// If you think you know better (you probably don't) you may use this method to break this rule.
    pub fn dictionary_id_nonsense_override(&mut self, id: Option<u32>) -> &mut Self {
        self.dictionary_id = id;
        self
    }

    #[throws(io::Error)]
    pub fn compress<R: Read, W: Write>(&self, reader: R, writer: W) {
        self.compress_internal(reader, writer, None)?;
    }

    #[throws(io::Error)]
    pub fn compress_with_size_unchecked<R: Read, W: Write>(&self, reader: R, writer: W, content_size: u64) {
        self.compress_internal(reader, writer, Some(content_size))?;
    }

    #[throws(io::Error)]
    pub fn compress_with_size<R: Read + Seek, W: Write>(&self, mut reader: R, writer: W) {
        // maybe one day we can just use reader.stream_len() here: https://github.com/rust-lang/rust/issues/59359
        // then again, we implement this to ignore the all bytes before the cursor which stream_len() does not
        let start = reader.seek(SeekFrom::Current(0))?;
        let end = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(start))?;

        let length = end - start;
        self.compress_internal(reader, writer, Some(length))?;
    }

    #[throws(io::Error)]
    fn compress_internal<R: Read, W: Write>(&self, mut reader: R, mut writer: W, content_size: Option<u64>) {
        let mut content_hasher = None;

        let mut flags = Flags::empty();
        if self.independent_blocks {
            flags |= Flags::IndependentBlocks;
        }
        if self.block_checksums {
            flags |= Flags::BlockChecksums;
        }
        if self.content_checksum {
            flags |= Flags::ContentChecksum;
            content_hasher = Some(XxHash32::with_seed(0));
        }
        if self.dictionary_id.is_some() { // TODO FIXME
            flags |= Flags::DictionaryId;
        }
        if content_size.is_some() {
            flags |= Flags::ContentSize;
        }

        let version = 1 << 6;
        let flag_byte = version | flags.bits();
        let bd_byte = BlockDescriptor::new(self.block_size).0;

        let mut header = Vec::new();
        header.write_u32::<LE>(MAGIC)?;
        header.write_u8(flag_byte)?;
        header.write_u8(bd_byte)?;
        
        if flags.contains(Flags::ContentSize) {
            header.write_u64::<LE>(content_size.unwrap())?;
        }
        if let Some(id) = self.dictionary_id {
            header.write_u32::<LE>(id)?;
        }

        let mut hasher = XxHash32::with_seed(0);
        hasher.write(&header[4..]); // skip magic for header checksum
        header.write_u8((hasher.finish() >> 8) as u8)?;
        writer.write_all(&header)?;

        let mut template_table = U32Table::default();
        let mut block_initializer: &[u8] = &[];
        if let Some(dict) = self.dictionary {
            for window in dict.windows(std::mem::size_of::<usize>()).step_by(3) {
                template_table.replace(dict, window.as_ptr() as usize - dict.as_ptr() as usize);
            }

            block_initializer = dict;
        }

        // TODO: when doing dependent blocks or dictionaries, in_buffer's capacity is insufficient
        let mut in_buffer = Vec::with_capacity(self.block_size);
        in_buffer.extend_from_slice(block_initializer);
        let mut out_buffer = vec![0u8; self.block_size];
        let mut table = template_table.clone();
        loop {
            let window_offset = in_buffer.len();

            // We basically want read_exact semantics, except at the end.
            // Sadly read_exact specifies the buffer contents to be undefined
            // on error, so we have to use this construction instead.
            reader.by_ref().take(self.block_size as u64).read_to_end(&mut in_buffer)?;
            let read_bytes = in_buffer.len() - window_offset;
            if read_bytes == 0 {
                break;
            }
            
            if let Some(x) = content_hasher.as_mut() {
                x.write(&in_buffer[window_offset..]);
            }

            // TODO: implement u16 table for small inputs

            // 1. limit output by input size so we never have negative compression ratio
            // 2. use a wrapper that forbids partial writes, so don't write 32-bit integers
            //    as four individual bytes with four individual range checks
            let mut cursor = NoPartialWrites(&mut out_buffer[..read_bytes]);
            let write = match compress2(&in_buffer, window_offset, &mut table, &mut cursor) {
                Ok(()) => {
                    let not_written_len = cursor.0.len();
                    let written_len = read_bytes - not_written_len;
                    writer.write_u32::<LE>(written_len as u32)?;
                    &out_buffer[..written_len]
                }
                Err(e) => {
                    assert!(e.kind() == ErrorKind::ConnectionAborted);
                    // incompressible
                    writer.write_u32::<LE>((read_bytes as u32) | INCOMPRESSIBLE)?;
                    &in_buffer[..read_bytes]
                }
            };

            writer.write_all(write)?;
            if flags.contains(Flags::BlockChecksums) {
                let mut block_hasher = XxHash32::with_seed(0);
                block_hasher.write(write);
                writer.write_u32::<LE>(block_hasher.finish() as u32)?;
            }

            if flags.contains(Flags::IndependentBlocks) {
                // clear table
                in_buffer.clear();
                in_buffer.extend_from_slice(block_initializer);

                table = template_table.clone();
            } else {
                if in_buffer.len() > WINDOW_SIZE {
                    let how_much_to_forget = in_buffer.len() - WINDOW_SIZE;
                    table.offset(how_much_to_forget);
                    in_buffer.drain(..how_much_to_forget);
                }
            }
        }
        writer.write_u32::<LE>(0)?;

        if let Some(x) = content_hasher {
            writer.write_u32::<LE>(x.finish() as u32)?;
        }
    }
}

struct NoPartialWrites<'a>(&'a mut [u8]);
impl<'a> Write for NoPartialWrites<'a> {
    #[inline]
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.0.len() < data.len() {
            // quite frankly it doesn't matter what we specify here
            return Err(ErrorKind::ConnectionAborted.into());
        }

        let amt = data.len();
        let (a, b) = mem::replace(&mut self.0, &mut []).split_at_mut(data.len());
        a.copy_from_slice(data);
        self.0 = b;
        Ok(amt)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

