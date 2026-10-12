//! The tag bound: the limit on the bytes of one file that a tag read takes
//! (`docs/design.md` section 3.10, "The tag bound").
//!
//! lofty has no option that limits the bytes it reads from a file. Its
//! allocation limit holds for one item of a tag and not for their number.
//! The bound is therefore built here, in two parts that share one limit:
//!
//! - [`declared`] reads the container headers of a file and adds up the
//!   sizes that they declare for the parts that lofty reads into memory. A
//!   file whose headers declare more than the limit is refused before lofty
//!   sees it, so nothing of its tag is read and nothing is allocated for it.
//! - [`Capped`] is the reader through which lofty reads the file. It hands
//!   out the limit and no byte more. It holds for every container, also for
//!   one without a rule in [`declared`], and for a file whose headers
//!   declare less than it holds.

use std::io::{self, Read, Seek, SeekFrom};

use lofty::file::FileType;

/// The most bytes of one file that a tag read takes: 32 MiB. It is twice the
/// limit of lofty for one item of a tag, which is the limit of the cover
/// service for one picture (`covers::MAX_SOURCE_BYTES`).
pub const MAX_TAG_BYTES: u64 = 32 * 1024 * 1024;

/// The most headers that [`declared`] reads from one file. A walk that needs
/// more stops, and [`Capped`] alone bounds the read of that file.
const MAX_HEADERS: u32 = 4096;

/// The type of a FLAC metadata block that holds the Vorbis comments.
const FLAC_VORBIS_COMMENT: u8 = 4;

/// The type of a FLAC metadata block that holds a picture.
const FLAC_PICTURE: u8 = 6;

/// The chunks of a WAV file that lofty reads into memory.
const WAV_CHUNKS: [&[u8; 4]; 4] = [b"LIST", b"ID3 ", b"id3 ", b"fmt "];

/// The chunks of an AIFF file that lofty reads into memory.
const AIFF_CHUNKS: [&[u8; 4]; 8] = [
    b"ID3 ", b"id3 ", b"NAME", b"AUTH", b"(c) ", b"ANNO", b"COMT", b"COMM",
];

/// A reader that hands out at most a fixed number of bytes of `inner`.
///
/// A read past that number fails, and so does every read after it.
/// [`Capped::exceeded`] then returns true. The end of `inner` is no read
/// past the number. Seeking is not limited, because a seek reads nothing.
pub(super) struct Capped<R> {
    inner: R,
    /// The bytes that the reader still hands out.
    left: u64,
    exceeded: bool,
}

impl<R> Capped<R> {
    /// Returns a reader that hands out at most `limit` bytes of `inner`.
    pub(super) fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            left: limit,
            exceeded: false,
        }
    }

    /// Returns whether a read asked for a byte past the limit.
    pub(super) fn exceeded(&self) -> bool {
        self.exceeded
    }

    /// Records the read past the limit and returns its error.
    fn refuse(&mut self) -> io::Error {
        self.exceeded = true;
        io::Error::other("the read is over the tag bound")
    }
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.exceeded {
            return Err(self.refuse());
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // The limit is used up. One more byte decides whether the caller
            // is at the end of the file or asks for a byte past the limit.
            let mut next = [0];
            return match self.inner.read(&mut next)? {
                0 => Ok(0),
                _ => Err(self.refuse()),
            };
        }
        let allowed = usize::try_from(self.left).map_or(buf.len(), |left| left.min(buf.len()));
        let read = self.inner.read(&mut buf[..allowed])?;
        self.left = self.left.saturating_sub(read as u64);
        Ok(read)
    }
}

impl<R: Seek> Seek for Capped<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }

    // A `BufReader` answers these two without dropping its buffer, so they
    // are passed on and not built from `seek`.
    fn stream_position(&mut self) -> io::Result<u64> {
        self.inner.stream_position()
    }

    fn seek_relative(&mut self, offset: i64) -> io::Result<()> {
        self.inner.seek_relative(offset)
    }
}

/// Returns the number of bytes that the headers of the file in `reader`
/// declare for the parts that lofty reads into memory, for a file that lofty
/// reads as `file_type`. The result is over `limit` when the headers declare
/// more than `limit`; the walk stops there, so the result is then a lower
/// bound of what they declare.
///
/// The function reads headers only: a few bytes each, and at most
/// [`MAX_HEADERS`] of them. It reads no byte of a tag, a picture or the
/// audio. It stops without an error at the first header that it cannot read
/// or does not know, and returns what it has added up to there. A file that
/// it does not measure in full is still read through [`Capped`].
///
/// What is counted for each container:
///
/// - FLAC: an ID3v2 tag before the stream marker, the first metadata block,
///   and every block of the types `VORBIS_COMMENT` and `PICTURE`.
/// - MP3 and ADTS AAC: the ID3v2 tags at the start of the file, the ID3v1
///   tag and the APE tag at its end.
/// - MP4: the first `moov` atom, which holds the `ilst` tag and the sample
///   tables.
/// - Ogg Vorbis, Opus and Speex: the header packets of the stream, which are
///   three for Vorbis and two for the others. The comment packet is one of
///   them.
/// - WAV: the chunks `LIST`, `ID3 `, `id3 ` and `fmt `.
/// - AIFF: the chunks `ID3 `, `id3 `, `NAME`, `AUTH`, `(c) `, `ANNO`, `COMT`
///   and `COMM`.
/// - Any other type that lofty detects (Monkey's Audio, Musepack, WavPack):
///   nothing.
pub(super) fn declared<R: Read + Seek>(reader: &mut R, file_type: FileType, limit: u64) -> u64 {
    let mut walk = Walk {
        reader,
        limit,
        declared: 0,
        headers_left: MAX_HEADERS,
    };
    // `None` says that the walk stopped early. What it added up counts.
    let _stopped_early = match file_type {
        FileType::Flac => walk.flac(),
        FileType::Mpeg | FileType::Aac => walk.id3_and_ape(),
        FileType::Mp4 => walk.mp4(),
        FileType::Vorbis => walk.ogg(3),
        FileType::Opus | FileType::Speex => walk.ogg(2),
        FileType::Wav => walk.chunks(b"RIFF", u32::from_le_bytes, &WAV_CHUNKS),
        FileType::Aiff => walk.chunks(b"FORM", u32::from_be_bytes, &AIFF_CHUNKS),
        _ => None,
    };
    walk.declared
}

/// The state of one call of [`declared`]. Each method returns `None` when
/// the walk stops: a header cannot be read or is not the expected one, the
/// headers declare more than the limit, or [`MAX_HEADERS`] are read.
struct Walk<'a, R> {
    reader: &'a mut R,
    limit: u64,
    declared: u64,
    headers_left: u32,
}

impl<R: Read + Seek> Walk<'_, R> {
    /// Adds `bytes` to the declared size. Returns `None` when the size is
    /// then over the limit.
    fn add(&mut self, bytes: u64) -> Option<()> {
        self.declared = self.declared.saturating_add(bytes);
        (self.declared <= self.limit).then_some(())
    }

    /// Reads a header of `N` bytes at the position of the reader.
    fn header<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.headers_left = self.headers_left.checked_sub(1)?;
        let mut bytes = [0; N];
        self.reader.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }

    /// Moves the reader `bytes` forward. The position can then be past the
    /// end of the file, where the next header cannot be read.
    fn skip(&mut self, bytes: u64) -> Option<()> {
        self.reader.seek_relative(i64::try_from(bytes).ok()?).ok()
    }

    /// Moves the reader to `position`, counted from the start of the file.
    fn seek_to(&mut self, position: u64) -> Option<()> {
        self.reader.seek(SeekFrom::Start(position)).ok().map(drop)
    }

    /// Returns the length of the file.
    fn file_len(&mut self) -> Option<u64> {
        self.reader.seek(SeekFrom::End(0)).ok()
    }

    /// Adds the ID3v2 tags that follow each other at `position` and returns
    /// the position after the last one.
    fn id3v2_tags(&mut self, mut position: u64) -> Option<u64> {
        loop {
            self.seek_to(position)?;
            let Some(size) = self.header::<10>().as_ref().and_then(id3v2_size) else {
                return Some(position);
            };
            self.add(size)?;
            position = position.checked_add(size)?;
        }
    }

    /// The walk of a FLAC file.
    fn flac(&mut self) -> Option<()> {
        let stream = self.id3v2_tags(0)?;
        self.seek_to(stream)?;
        if &self.header::<4>()? != b"fLaC" {
            return None;
        }
        let mut first = true;
        loop {
            let [flags, size @ ..] = self.header::<4>()?;
            let size = u64::from(u32::from_be_bytes([0, size[0], size[1], size[2]]));
            // lofty reads the first block whatever its type, and takes the
            // stream information from it.
            if first || matches!(flags & 0x7f, FLAC_VORBIS_COMMENT | FLAC_PICTURE) {
                self.add(size)?;
            }
            first = false;
            if flags & 0x80 != 0 {
                return Some(());
            }
            self.skip(size)?;
        }
    }

    /// The walk of a file whose tags are ID3v2 tags at its start and an
    /// ID3v1 tag and an APE tag at its end: MP3 and ADTS AAC.
    fn id3_and_ape(&mut self) -> Option<()> {
        self.id3v2_tags(0)?;
        let mut end = self.file_len()?;
        if let Some(start) = end.checked_sub(128) {
            self.seek_to(start)?;
            if &self.header::<3>()? == b"TAG" {
                self.add(128)?;
                end = start;
            }
        }
        // The footer of an APE tag: the preamble, the version, and the size
        // of the items and the footer.
        let footer = end.checked_sub(32)?;
        self.seek_to(footer)?;
        let [preamble @ .., a, b, c, d] = self.header::<16>()?;
        if &preamble[..8] == b"APETAGEX" {
            self.add(u64::from(u32::from_le_bytes([a, b, c, d])))?;
        }
        Some(())
    }

    /// The walk of an MP4 file: the atoms of the top level up to the first
    /// `moov` atom.
    fn mp4(&mut self) -> Option<()> {
        let len = self.file_len()?;
        let mut position = 0u64;
        loop {
            self.seek_to(position)?;
            let [a, b, c, d, name @ ..] = self.header::<8>()?;
            let size = match u32::from_be_bytes([a, b, c, d]) {
                // The atom ends with the file.
                0 => len.checked_sub(position)?,
                // The size is the 64-bit number after the name.
                1 => u64::from_be_bytes(self.header::<8>()?),
                size => u64::from(size),
            };
            if &name == b"moov" {
                return self.add(size);
            }
            if size < 8 {
                return None;
            }
            position = position.checked_add(size)?;
        }
    }

    /// The walk of an Ogg file: the pages that hold the first `packets`
    /// packets. The segment table of a page gives the sizes of the packets,
    /// so no page content is read.
    fn ogg(&mut self, packets: u32) -> Option<()> {
        self.seek_to(0)?;
        let mut complete = 0;
        loop {
            let page = self.header::<27>()?;
            if &page[..4] != b"OggS" {
                return None;
            }
            let mut table = [0; 255];
            let table = &mut table[..usize::from(page[26])];
            self.reader.read_exact(table).ok()?;
            let mut content = 0;
            for &segment in &*table {
                self.add(u64::from(segment))?;
                content += u64::from(segment);
                // A segment of less than 255 bytes ends a packet.
                if segment < 255 {
                    complete += 1;
                    if complete == packets {
                        return Some(());
                    }
                }
            }
            self.skip(content)?;
        }
    }

    /// The walk of a RIFF or an IFF file whose first four bytes are `form`:
    /// the chunks of the top level. `number` reads the size of a chunk, in
    /// the byte order of the container. The chunks named in `counted` are
    /// added.
    fn chunks(
        &mut self,
        form: &[u8; 4],
        number: fn([u8; 4]) -> u32,
        counted: &[&[u8; 4]],
    ) -> Option<()> {
        self.seek_to(0)?;
        let [a, b, c, d, ..] = self.header::<12>()?;
        if &[a, b, c, d] != form {
            return None;
        }
        loop {
            let [name @ .., a, b, c, d] = self.header::<8>()?;
            let size = u64::from(number([a, b, c, d]));
            if counted.contains(&&name) {
                self.add(size)?;
            }
            // A chunk of an odd size is followed by one byte of padding.
            self.skip(size + size % 2)?;
        }
    }
}

/// Returns the size of the ID3v2 tag whose first ten bytes are `header`: the
/// header, the frames and the padding, and the footer when the header names
/// one. Returns `None` when the bytes do not start an ID3v2 tag.
fn id3v2_size(header: &[u8; 10]) -> Option<u64> {
    let [b'I', b'D', b'3', _, _, flags, size @ ..] = header else {
        return None;
    };
    // Each byte holds seven bits of the size. lofty drops the eighth bit.
    let frames = size
        .iter()
        .fold(0u64, |size, byte| (size << 7) | u64::from(byte & 0x7f));
    let footer = if flags & 0x10 == 0 { 0 } else { 10 };
    Some(10 + frames + footer)
}

/// A reader for the tests of the tag bound.
#[cfg(test)]
pub(super) mod testing {
    use std::io::{self, Read, Seek, SeekFrom};

    /// A reader that records what it is asked to read. It is the proof that
    /// a refused file was not read: a test asserts on the bytes that the
    /// reader handed out and on the longest buffer that one read gave it.
    pub struct Counting<R> {
        inner: R,
        /// The bytes that the reader handed out.
        pub bytes: u64,
        /// The longest buffer of one read.
        pub longest: usize,
    }

    impl<R> Counting<R> {
        pub fn new(inner: R) -> Self {
            Self {
                inner,
                bytes: 0,
                longest: 0,
            }
        }
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.longest = self.longest.max(buf.len());
            let read = self.inner.read(buf)?;
            self.bytes += read as u64;
            Ok(read)
        }
    }

    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::testing::Counting;
    use super::*;

    /// Returns what [`declared`] returns for `bytes` with a limit that no
    /// file reaches, and checks that the walk read headers only: at most
    /// `headers` bytes in reads of at most 255 bytes.
    fn declared_of(bytes: &[u8], file_type: FileType, headers: u64) -> u64 {
        let mut reader = Counting::new(Cursor::new(bytes));
        let size = declared(&mut reader, file_type, u64::MAX);
        assert!(reader.bytes <= headers, "{} bytes read", reader.bytes);
        assert!(reader.longest <= 255, "a read of {} bytes", reader.longest);
        size
    }

    /// The header of an ID3v2.4 tag whose frames and padding take `size`
    /// bytes.
    fn id3v2_header(size: u32) -> [u8; 10] {
        let seven = |shift: u32| ((size >> shift) & 0x7f) as u8;
        [
            b'I',
            b'D',
            b'3',
            4,
            0,
            0,
            seven(21),
            seven(14),
            seven(7),
            seven(0),
        ]
    }

    /// The header of a FLAC metadata block.
    fn flac_block(kind: u8, last: bool, size: u32) -> [u8; 4] {
        let size = size.to_be_bytes();
        [
            kind | if last { 0x80 } else { 0 },
            size[1],
            size[2],
            size[3],
        ]
    }

    /// An Ogg page header with `segments` as its segment table.
    fn ogg_page(segments: &[u8]) -> Vec<u8> {
        let mut page = b"OggS".to_vec();
        page.resize(26, 0);
        page.push(u8::try_from(segments.len()).expect("at most 255 segments"));
        page.extend(segments);
        page
    }

    /// An MP4 atom header.
    fn atom(name: &[u8; 4], size: u32) -> Vec<u8> {
        [&size.to_be_bytes()[..], name].concat()
    }

    /// A chunk header of a RIFF file.
    fn riff_chunk(name: &[u8; 4], size: u32) -> Vec<u8> {
        [&name[..], &size.to_le_bytes()].concat()
    }

    /// A chunk header of an IFF file.
    fn iff_chunk(name: &[u8; 4], size: u32) -> Vec<u8> {
        [&name[..], &size.to_be_bytes()].concat()
    }

    #[test]
    fn the_capped_reader_hands_out_the_limit_and_fails_after_it() {
        let mut reader = Capped::new(Cursor::new(vec![7u8; 100]), 10);
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 8);
        // A read that asks for more than is left gets what is left.
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert!(!reader.exceeded());

        let error = reader.read(&mut buf).expect_err("the limit is used up");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(reader.exceeded());
        // Every later read fails too, also after a seek to the start.
        reader.seek(SeekFrom::Start(0)).unwrap();
        assert!(reader.read(&mut buf).is_err());
        assert!(reader.read_exact(&mut buf[..1]).is_err());
    }

    #[test]
    fn the_end_of_the_file_is_no_read_past_the_limit() {
        let mut reader = Capped::new(Cursor::new(vec![7u8; 10]), 10);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("ten bytes");
        assert_eq!(bytes, [7; 10]);
        assert!(!reader.exceeded());

        // A file of eleven bytes has one byte past the limit.
        let mut reader = Capped::new(Cursor::new(vec![7u8; 11]), 10);
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        assert!(reader.exceeded());
    }

    #[test]
    fn the_capped_reader_does_not_limit_seeks_and_an_empty_read() {
        let mut reader = Capped::new(Cursor::new(vec![7u8; 100]), 4);
        assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), 100);
        reader.seek_relative(-50).unwrap();
        assert_eq!(reader.stream_position().unwrap(), 50);
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).expect("the four bytes at 50");
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        assert!(!reader.exceeded());
    }

    #[test]
    fn an_id3v2_header_declares_its_frames_its_header_and_its_footer() {
        assert_eq!(id3v2_size(&id3v2_header(0)), Some(10));
        assert_eq!(id3v2_size(&id3v2_header(257)), Some(267));
        // The largest size that four bytes of seven bits hold.
        assert_eq!(
            id3v2_size(&[b'I', b'D', b'3', 3, 0, 0, 0x7f, 0x7f, 0x7f, 0x7f]),
            Some(10 + (1 << 28) - 1)
        );
        // The eighth bit of a size byte is dropped, as lofty drops it.
        assert_eq!(
            id3v2_size(&[b'I', b'D', b'3', 4, 0, 0, 0x80, 0x80, 0x80, 0x81]),
            Some(11)
        );
        let mut with_footer = id3v2_header(100);
        with_footer[5] = 0x10;
        assert_eq!(id3v2_size(&with_footer), Some(120));
        assert_eq!(id3v2_size(b"fLaC\0\0\0\x22\x10\0"), None);
        assert_eq!(id3v2_size(&[0; 10]), None);
    }

    #[test]
    fn a_flac_file_declares_its_first_block_its_comments_and_its_pictures() {
        let mut file = b"fLaC".to_vec();
        file.extend(flac_block(0, false, 34));
        file.resize(file.len() + 34, 0);
        // A padding block and a seek table are not read into memory.
        file.extend(flac_block(1, false, 5000));
        file.resize(file.len() + 5000, 0);
        file.extend(flac_block(3, false, 180));
        file.resize(file.len() + 180, 0);
        file.extend(flac_block(FLAC_VORBIS_COMMENT, false, 700));
        file.resize(file.len() + 700, 0);
        file.extend(flac_block(FLAC_PICTURE, false, 9000));
        file.resize(file.len() + 9000, 0);
        file.extend(flac_block(FLAC_PICTURE, true, 11));
        file.resize(file.len() + 11 + 4096, 0xff);

        // Ten bytes that are no ID3v2 header, the stream marker and six
        // block headers.
        assert_eq!(declared_of(&file, FileType::Flac, 38), 34 + 700 + 9000 + 11);

        // An ID3v2 tag before the stream marker is counted with its header.
        let mut tagged = id3v2_header(300).to_vec();
        tagged.resize(310, 0);
        tagged.extend(&file);
        assert_eq!(
            declared_of(&tagged, FileType::Flac, 48),
            310 + 34 + 700 + 9000 + 11
        );
    }

    #[test]
    fn flac_blocks_that_are_not_there_are_declared_all_the_same() {
        // Three picture blocks of the largest size. The file ends after the
        // header of the first one, so the walk reads two headers.
        let mut file = b"fLaC".to_vec();
        file.extend(flac_block(0, false, 34));
        file.resize(file.len() + 34, 0);
        file.extend(flac_block(FLAC_PICTURE, false, 0xff_ffff));
        assert_eq!(declared_of(&file, FileType::Flac, 22), 34 + 0xff_ffff);

        // A first block that is no stream information is read by lofty too.
        let mut file = b"fLaC".to_vec();
        file.extend(flac_block(1, false, 0xff_ffff));
        assert_eq!(declared_of(&file, FileType::Flac, 16), 0xff_ffff);
    }

    #[test]
    fn an_mp3_file_declares_its_id3v2_tags_its_id3v1_tag_and_its_ape_tag() {
        // Two ID3v2 tags, audio, an APE tag with its footer, an ID3v1 tag.
        let mut file = id3v2_header(1000).to_vec();
        file.resize(1010, 0);
        file.extend(id3v2_header(50));
        file.resize(file.len() + 50, 0);
        file.resize(file.len() + 4000, 0xaa);
        file.resize(file.len() + 600, 1);
        file.extend(b"APETAGEX");
        file.extend(2000u32.to_le_bytes());
        file.extend(632u32.to_le_bytes());
        file.resize(file.len() + 16, 0);
        file.extend(b"TAG");
        file.resize(file.len() + 125, 0);

        for file_type in [FileType::Mpeg, FileType::Aac] {
            // Three headers of ten bytes, the mark of ID3v1, the footer.
            assert_eq!(
                declared_of(&file, file_type, 49),
                1010 + 60 + 128 + 632,
                "{file_type:?}"
            );
        }

        // A file without a tag declares nothing.
        assert_eq!(declared_of(&[0xaa; 5000], FileType::Mpeg, 29), 0);
        assert_eq!(declared_of(&[], FileType::Mpeg, 0), 0);
    }

    #[test]
    fn an_id3v2_header_declares_a_tag_that_is_not_there() {
        // Ten bytes that declare a tag of 256 MiB.
        let file = [b'I', b'D', b'3', 4, 0, 0, 0x7f, 0x7f, 0x7f, 0x7f];
        let size = 10 + (1 << 28) - 1;
        assert_eq!(declared_of(&file, FileType::Mpeg, 26), size);
        assert_eq!(declared_of(&file, FileType::Flac, 10), size);
        assert!(size > MAX_TAG_BYTES);
    }

    #[test]
    fn an_mp4_file_declares_its_moov_atom() {
        let mut file = atom(b"ftyp", 24);
        file.resize(24, 0);
        file.extend(atom(b"free", 100));
        file.resize(file.len() + 92, 0);
        // An atom whose size is a 64-bit number.
        file.extend(atom(b"mdat", 1));
        file.extend(3016u64.to_be_bytes());
        file.resize(file.len() + 3000, 0xaa);
        file.extend(atom(b"moov", 70_000));
        assert_eq!(declared_of(&file, FileType::Mp4, 40), 70_000);

        // The `moov` atom of a file that ends after the header of the atom.
        let mut file = atom(b"ftyp", 16);
        file.resize(16, 0);
        file.extend(atom(b"moov", u32::MAX));
        assert_eq!(declared_of(&file, FileType::Mp4, 16), u64::from(u32::MAX));

        // A `moov` atom that ends with the file.
        let mut file = atom(b"ftyp", 16);
        file.resize(16, 0);
        file.extend(atom(b"moov", 0));
        file.resize(file.len() + 500, 0);
        assert_eq!(declared_of(&file, FileType::Mp4, 16), 508);

        // No `moov` atom, and an atom that is shorter than its header.
        let mut file = atom(b"ftyp", 16);
        file.resize(16, 0);
        file.extend(atom(b"free", 4));
        file.extend(atom(b"moov", 900));
        assert_eq!(declared_of(&file, FileType::Mp4, 16), 0);
    }

    #[test]
    fn an_ogg_file_declares_its_header_packets() {
        // The identification packet on a page of its own. The comment packet
        // over two pages: 255 + 255 bytes, then 255 + 10. The setup packet
        // and the first audio packet on the next page.
        let mut file = ogg_page(&[30]);
        file.resize(file.len() + 30, 0);
        file.extend(ogg_page(&[255, 255]));
        file.resize(file.len() + 510, 0);
        file.extend(ogg_page(&[255, 10]));
        file.resize(file.len() + 265, 0);
        file.extend(ogg_page(&[200, 99]));
        file.resize(file.len() + 299, 0);
        file.extend(ogg_page(&[255, 255, 255]));
        file.resize(file.len() + 765, 0);

        let headers = 4 * 27 + 7;
        assert_eq!(
            declared_of(&file, FileType::Vorbis, headers),
            30 + 775 + 200
        );
        for file_type in [FileType::Opus, FileType::Speex] {
            assert_eq!(
                declared_of(&file, file_type, 3 * 27 + 5),
                30 + 775,
                "{file_type:?}"
            );
        }

        // A comment packet that never ends is declared page by page. The
        // file ends after the header of its first page.
        let mut file = ogg_page(&[30]);
        file.resize(file.len() + 30, 0);
        file.extend(ogg_page(&[255; 255]));
        assert_eq!(
            declared_of(&file, FileType::Opus, 2 * 27 + 256),
            30 + 255 * 255
        );
        assert_eq!(
            declared_of(b"not an Ogg page, 27 bytes..", FileType::Opus, 27),
            0
        );
    }

    #[test]
    fn a_wav_file_declares_its_tag_chunks_and_its_format_chunk() {
        let mut file = riff_chunk(b"RIFF", 0);
        file.extend(b"WAVE");
        file.extend(riff_chunk(b"fmt ", 16));
        file.resize(file.len() + 16, 0);
        // The samples are not read into memory. The odd size has one byte
        // of padding.
        file.extend(riff_chunk(b"data", 801));
        file.resize(file.len() + 802, 0xaa);
        file.extend(riff_chunk(b"LIST", 300));
        file.resize(file.len() + 300, 0);
        file.extend(riff_chunk(b"bext", 602));
        file.resize(file.len() + 602, 0);
        file.extend(riff_chunk(b"id3 ", 5000));

        assert_eq!(declared_of(&file, FileType::Wav, 52), 16 + 300 + 5000);
        // The bytes are no AIFF file.
        assert_eq!(declared_of(&file, FileType::Aiff, 12), 0);
    }

    #[test]
    fn an_aiff_file_declares_its_tag_chunks_and_its_common_chunk() {
        let mut file = iff_chunk(b"FORM", 0);
        file.extend(b"AIFF");
        file.extend(iff_chunk(b"COMM", 18));
        file.resize(file.len() + 18, 0);
        file.extend(iff_chunk(b"SSND", 1608));
        file.resize(file.len() + 1608, 0xaa);
        for (name, size) in [(b"NAME", 7u32), (b"AUTH", 20), (b"(c) ", 4), (b"ANNO", 90)] {
            file.extend(iff_chunk(name, size));
            file.resize(file.len() + (size + size % 2) as usize, 0);
        }
        file.extend(iff_chunk(b"COMT", 40));
        file.resize(file.len() + 40, 0);
        file.extend(iff_chunk(b"ID3 ", 0x7fff_ffff));

        assert_eq!(
            declared_of(&file, FileType::Aiff, 76),
            18 + 7 + 20 + 4 + 90 + 40 + 0x7fff_ffff
        );
        assert_eq!(declared_of(&file, FileType::Wav, 12), 0);
    }

    #[test]
    fn the_walk_stops_when_the_headers_declare_more_than_the_limit() {
        // Four picture blocks of 1000 bytes. With a limit of 2500 bytes the
        // walk stops at the third one and does not read the fourth header.
        let mut file = b"fLaC".to_vec();
        file.extend(flac_block(0, false, 34));
        file.resize(file.len() + 34, 0);
        for index in 0..4 {
            file.extend(flac_block(FLAC_PICTURE, index == 3, 1000));
            file.resize(file.len() + 1000, 0);
        }
        let mut reader = Counting::new(Cursor::new(&file));
        let size = declared(&mut reader, FileType::Flac, 2500);
        assert_eq!(size, 34 + 3000);
        // Ten bytes that are no ID3v2 header, the stream marker and the
        // headers of four blocks.
        assert_eq!(reader.bytes, 10 + 4 + 4 * 4);
    }

    #[test]
    fn the_walk_reads_a_fixed_number_of_headers_at_most() {
        // More empty blocks than the walk reads headers. The last one is a
        // picture block that the walk does not reach.
        let mut file = b"fLaC".to_vec();
        for _ in 0..MAX_HEADERS + 10 {
            file.extend(flac_block(1, false, 0));
        }
        file.extend(flac_block(FLAC_PICTURE, true, 5000));
        let mut reader = Counting::new(Cursor::new(&file));
        assert_eq!(declared(&mut reader, FileType::Flac, u64::MAX), 0);
        // The ten bytes that are no ID3v2 header and the stream marker are
        // two of the headers.
        assert_eq!(reader.bytes, 10 + 4 + u64::from(MAX_HEADERS - 2) * 4);
    }

    #[test]
    fn a_type_without_a_rule_declares_nothing_and_is_not_read() {
        let file = [b'I', b'D', b'3', 4, 0, 0, 0x7f, 0x7f, 0x7f, 0x7f];
        for file_type in [FileType::Ape, FileType::Mpc, FileType::WavPack] {
            assert_eq!(declared_of(&file, file_type, 0), 0, "{file_type:?}");
        }
    }
}
