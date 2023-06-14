use byteorder::{BigEndian, ReadBytesExt};
use crc32c::crc32c;
use std::io::{Cursor, Read, Seek, SeekFrom};

use crate::{constants::COUCH_BLOCK_SIZE, TreeFile};

impl TreeFile {
    pub fn pread_compressed(&mut self, pos: usize) -> Vec<u8> {
        let compressed_buf = self.pread_bin_internal(pos, None);

        // Couchstore does not use the frame format so we need the raw decoder.
        let decompressed_buf = snap::raw::Decoder::new()
            .decompress_vec(&compressed_buf)
            .unwrap();

        return decompressed_buf;
    }

    pub fn pread_bin(&mut self, pos: usize) -> Vec<u8> {
        return self.pread_bin_internal(pos, None);
    }

    fn pread_bin_internal(&mut self, mut pos: usize, max_header_size: Option<usize>) -> Vec<u8> {
        let mut info = [0u8; 8];

        self.read_skipping_prefixes(&mut pos, &mut info);

        let mut cursor = Cursor::new(&info);
        // something is stored in the highest bit of the first byte
        let mut chunk_len = cursor.read_u32::<BigEndian>().unwrap() & !0x80000000;
        let crc32 = cursor.read_u32::<BigEndian>().unwrap();

        if let Some(max_header_size) = max_header_size {
            assert!(chunk_len as usize <= max_header_size);
            chunk_len -= 4; // Header len includes CRC len.
        }

        // TODO: Reuse buffer
        let mut buf = vec![0u8; chunk_len as usize];

        self.read_skipping_prefixes(&mut pos, &mut buf);

        // How does crc32c differ from crc32?
        let crc32_calc = crc32c(&buf);

        assert_eq!(crc32, crc32_calc);

        return buf;
    }

    pub fn pread_header(&mut self, pos: usize, max_header_size: Option<usize>) -> Vec<u8> {
        // TODO: make this more idiomatic
        if max_header_size.is_none() {
            panic!("max_header_size is None");
        }

        return self.pread_bin_internal(pos + 1, max_header_size);
    }

    pub fn read_skipping_prefixes(&mut self, pos: &mut usize, mut buf: &mut [u8]) {
        if *pos % COUCH_BLOCK_SIZE == 0 {
            *pos += 1;
        }

        while !buf.is_empty() {
            let mut read_size = COUCH_BLOCK_SIZE - (*pos % COUCH_BLOCK_SIZE);
            if read_size > buf.len() {
                read_size = buf.len();
            }

            self.file.seek(SeekFrom::Start(*pos as u64)).unwrap();
            let got_bytes = self.file.read(&mut buf[..read_size]).unwrap();

            if got_bytes == 0 {
                break;
            }

            *pos += got_bytes;

            buf = &mut buf[got_bytes..];

            if *pos % COUCH_BLOCK_SIZE == 0 {
                *pos += 1;
            }
        }
    }
}
