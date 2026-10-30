//! A ZIP archive's table of contents (its central directory), read from the end of the archive
//! without the rest of it, and single files of it unpacked as their bytes arrive. This is what
//! the Download File window's "What's inside" reads over range requests.
//!
//! Layout (APPNOTE.TXT): `[local header + data]… [central directory] [ZIP64 end record]
//! [ZIP64 locator] [end record + comment]`. The end record says where the directory is; the
//! ZIP64 records take over when an archive is too big for the end record's 32-bit fields.

use std::future::Future;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_END: u32 = 0x0605_4b50;
const SIG_ZIP64_END: u32 = 0x0606_4b50;
const SIG_ZIP64_LOCATOR: u32 = 0x0706_4b50;

/// The end record without its comment.
const END_LEN: usize = 22;
/// The ZIP64 end-of-directory locator.
const LOCATOR_LEN: usize = 20;
/// The ZIP64 end record without extensible data.
const ZIP64_END_LEN: usize = 56;
/// A central directory entry without its name, extra field and comment.
const CENTRAL_LEN: usize = 46;
/// A local file header without its name and extra field.
pub(crate) const LOCAL_LEN: u64 = 30;

/// The last bytes of an archive that hold its end: the end record with the longest comment,
/// and the ZIP64 records before it. Read in one go, they often hold the directory too.
pub(crate) const TAIL_LEN: u64 = (END_LEN + 0xFFFF + LOCATOR_LEN + ZIP64_END_LEN) as u64;
/// The largest directory read (about 150 000 files).
pub(crate) const MAX_DIRECTORY: u64 = 16 << 20;

pub(crate) const STORED: u16 = 0;
pub(crate) const DEFLATED: u16 = 8;
/// WinZip AES encryption.
const AES: u16 = 99;

/// Why a table of contents can't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZipError {
    /// No end record: not a ZIP archive.
    NotZip,
    /// The records contradict each other or end too early.
    Damaged,
    /// A split archive (`.z01`, `.z02`, … with the directory spread over the parts).
    Split,
    /// The directory is larger than [`MAX_DIRECTORY`].
    TooBig,
}

/// Where the central directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Directory {
    /// Absolute offset in the file.
    pub offset: u64,
    pub len: u64,
    /// Entries the end record counts.
    pub entries: u64,
    /// Bytes in front of the archive (a self-extracting program): the offsets the archive
    /// gives are this much too small.
    pub shift: u64,
}

/// What the end of an archive says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    Found(Directory),
    /// A ZIP64 archive whose ZIP64 end record (at this absolute offset) wasn't among the bytes
    /// read: read [`zip64_end`] from there.
    Zip64At(u64),
}

/// One entry of the central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Path inside the archive, with `/` between folders; folders end with `/`.
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc32: u32,
    pub compressed: u64,
    /// Unpacked size.
    pub size: u64,
    /// Absolute offset of the entry's local header.
    pub header: u64,
}

impl Entry {
    pub(crate) fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }

    /// Stored or deflated, and not encrypted: what [`Unpacker`] handles.
    pub(crate) fn can_unpack(&self) -> bool {
        self.flags & 1 == 0 && matches!(self.method, STORED | DEFLATED)
    }

    /// Encrypted (classic ZIP encryption or AES).
    pub(crate) fn encrypted(&self) -> bool {
        self.flags & 1 != 0 || self.method == AES
    }

    /// The last part of the path.
    pub(crate) fn file_name(&self) -> &str {
        self.name
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The end record in the last bytes of a file: the last signature whose comment reaches exactly
/// to the end (a signature inside the comment doesn't), else one followed by stray bytes.
fn end_record(tail: &[u8]) -> Option<usize> {
    let last = tail.len().checked_sub(END_LEN)?;
    let candidates = || {
        (0..=last)
            .rev()
            .filter(|&p| u32_at(tail, p) == Some(SIG_END))
    };
    let comment_end = |p: usize| p + END_LEN + u16_at(tail, p + 20).unwrap_or(0) as usize;
    candidates()
        .find(|&p| comment_end(p) == tail.len())
        .or_else(|| candidates().find(|&p| comment_end(p) <= tail.len()))
}

/// Finds the directory from the last bytes of a file (`tail`, which starts at `tail_start`).
pub(crate) fn find_end(tail: &[u8], tail_start: u64) -> Result<End, ZipError> {
    let pos = end_record(tail).ok_or(ZipError::NotZip)?;
    let end = &tail[pos..];
    let end_at = tail_start + pos as u64;
    if pos >= LOCATOR_LEN && u32_at(tail, pos - LOCATOR_LEN) == Some(SIG_ZIP64_LOCATOR) {
        let locator = &tail[pos - LOCATOR_LEN..pos];
        let disks = u32_at(locator, 16).ok_or(ZipError::Damaged)?;
        if u32_at(locator, 4) != Some(0) || disks > 1 {
            return Err(ZipError::Split);
        }
        let stated = u64_at(locator, 8).ok_or(ZipError::Damaged)?;
        let in_tail = |at: u64| {
            at.checked_sub(tail_start)
                .and_then(|i| tail.get(i as usize..))
                .filter(|b| b.len() >= ZIP64_END_LEN && u32_at(b, 0) == Some(SIG_ZIP64_END))
        };
        // Usually right before the locator; the locator's offset is too small by whatever a
        // self-extracting program in front of the archive takes.
        let before = (end_at - LOCATOR_LEN as u64).checked_sub(ZIP64_END_LEN as u64);
        let at = [Some(stated), before]
            .into_iter()
            .flatten()
            .find(|&at| in_tail(at).is_some());
        return match at.and_then(|at| Some((at, in_tail(at)?))) {
            Some((at, record)) => zip64_end(record, at).map(End::Found),
            None => Ok(End::Zip64At(stated)),
        };
    }
    let field = |at| u16_at(end, at).ok_or(ZipError::Damaged);
    let (disk, dir_disk) = (field(4)?, field(6)?);
    let (here, entries) = (field(8)?, field(10)?);
    if disk != 0 || dir_disk != 0 || here != entries {
        return Err(ZipError::Split);
    }
    let len = u32_at(end, 12).ok_or(ZipError::Damaged)? as u64;
    let offset = u32_at(end, 16).ok_or(ZipError::Damaged)? as u64;
    directory(offset, len, entries as u64, end_at).map(End::Found)
}

/// The directory ends where the record after it starts (`next_at`); any difference to what the
/// archive says is data in front of it.
fn directory(offset: u64, len: u64, entries: u64, next_at: u64) -> Result<Directory, ZipError> {
    let stated_end = offset.checked_add(len).ok_or(ZipError::Damaged)?;
    let shift = next_at.checked_sub(stated_end).ok_or(ZipError::Damaged)?;
    Ok(Directory {
        offset: offset + shift,
        len,
        entries,
        shift,
    })
}

/// Reads the ZIP64 end record (`record` holds the bytes from `at`, its absolute offset).
pub(crate) fn zip64_end(record: &[u8], at: u64) -> Result<Directory, ZipError> {
    if u32_at(record, 0) != Some(SIG_ZIP64_END) {
        return Err(ZipError::Damaged);
    }
    let field = |i| u64_at(record, i).ok_or(ZipError::Damaged);
    let (disk, dir_disk) = (
        u32_at(record, 16).ok_or(ZipError::Damaged)?,
        u32_at(record, 20).ok_or(ZipError::Damaged)?,
    );
    let (here, entries) = (field(24)?, field(32)?);
    if disk != 0 || dir_disk != 0 || here != entries {
        return Err(ZipError::Split);
    }
    directory(field(48)?, field(40)?, entries, at)
}

/// The data of extra field `id`.
fn extra_field(extra: &[u8], id: u16) -> Option<&[u8]> {
    let mut rest = extra;
    while rest.len() >= 4 {
        let (tag, len) = (u16_at(rest, 0)?, u16_at(rest, 2)? as usize);
        let data = rest.get(4..4 + len)?;
        if tag == id {
            return Some(data);
        }
        rest = &rest[4 + len..];
    }
    None
}

/// Code page 437 (the original IBM PC set), bytes 0x80–0xFF: names without the UTF-8 flag.
const CP437_HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐\
                          └┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";

/// An entry's path: UTF-8 when flagged (or from the Info-ZIP Unicode Path field written for
/// it), UTF-8 when it is valid UTF-8 anyway, else code page 437; `\` turned into `/`.
fn entry_name(raw: &[u8], flags: u16, extra: &[u8]) -> String {
    let unicode = extra_field(extra, 0x7075)
        .filter(|d| d.len() > 5 && d[0] == 1 && u32_at(d, 1) == Some(crc32fast::hash(raw)))
        .and_then(|d| std::str::from_utf8(&d[5..]).ok());
    let name = match (unicode, std::str::from_utf8(raw)) {
        (Some(name), _) => name.to_string(),
        (None, Ok(name)) => name.to_string(),
        (None, Err(_)) if flags & 0x800 != 0 => String::from_utf8_lossy(raw).into_owned(),
        (None, Err(_)) => raw
            .iter()
            .map(|&b| match b {
                0..=0x7f => b as char,
                _ => CP437_HIGH.chars().nth(b as usize - 0x80).unwrap_or('?'),
            })
            .collect(),
    };
    name.replace('\\', "/").trim_start_matches('/').to_string()
}

/// The entries of a central directory (`buf` holds all of it).
pub(crate) fn parse_directory(buf: &[u8], dir: &Directory) -> Result<Vec<Entry>, ZipError> {
    let mut entries = Vec::with_capacity(dir.entries.min(1 << 16) as usize);
    let mut pos = 0;
    while pos < buf.len() {
        let h = &buf[pos..];
        if u32_at(h, 0) != Some(SIG_CENTRAL) {
            // Anything after the last entry (a digital signature record) is not ours to read;
            // anything before it is damage.
            if (entries.len() as u64) < dir.entries {
                return Err(ZipError::Damaged);
            }
            break;
        }
        let short = |at| u16_at(h, at).ok_or(ZipError::Damaged);
        let long = |at| u32_at(h, at).ok_or(ZipError::Damaged);
        let (name_len, extra_len, comment_len) = (
            short(28)? as usize,
            short(30)? as usize,
            short(32)? as usize,
        );
        let name = h
            .get(CENTRAL_LEN..CENTRAL_LEN + name_len)
            .ok_or(ZipError::Damaged)?;
        let extra = h
            .get(CENTRAL_LEN + name_len..CENTRAL_LEN + name_len + extra_len)
            .ok_or(ZipError::Damaged)?;
        let flags = short(8)?;
        let (mut compressed, mut size, mut header) =
            (long(20)? as u64, long(24)? as u64, long(42)? as u64);
        // ZIP64 extended information: the 64-bit values of the fields that are all ones, in
        // this order.
        if let Some(mut wide) = extra_field(extra, 0x0001) {
            for field in [&mut size, &mut compressed, &mut header] {
                if *field == u32::MAX as u64 {
                    *field = u64_at(wide, 0).ok_or(ZipError::Damaged)?;
                    wide = &wide[8..];
                }
            }
        }
        entries.push(Entry {
            name: entry_name(name, flags, extra),
            method: short(10)?,
            flags,
            crc32: long(16)?,
            compressed,
            size,
            header: header.checked_add(dir.shift).ok_or(ZipError::Damaged)?,
        });
        pos += CENTRAL_LEN + name_len + extra_len + comment_len;
    }
    if entries.is_empty() && dir.entries > 0 {
        return Err(ZipError::Damaged);
    }
    Ok(entries)
}

/// Where an entry's data starts, from its local header (`header`, at least [`LOCAL_LEN`]
/// bytes from the entry's [`Entry::header`] offset).
pub(crate) fn data_offset(header: &[u8], entry: &Entry) -> Result<u64, ZipError> {
    if u32_at(header, 0) != Some(SIG_LOCAL) {
        return Err(ZipError::Damaged);
    }
    let name_len = u16_at(header, 26).ok_or(ZipError::Damaged)? as u64;
    let extra_len = u16_at(header, 28).ok_or(ZipError::Damaged)? as u64;
    Ok(entry.header + LOCAL_LEN + name_len + extra_len)
}

/// Something that reads byte ranges of a file (a server, or bytes in memory in the tests).
pub(crate) trait ReadAt {
    type Error;
    fn read_at(
        &mut self,
        from: u64,
        len: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadError<E> {
    Zip(ZipError),
    Source(E),
}

impl<E> From<ZipError> for ReadError<E> {
    fn from(e: ZipError) -> Self {
        ReadError::Zip(e)
    }
}

/// Reads the table of contents of a ZIP of `size` bytes from its end: its last bytes, then
/// (when they don't hold them) the ZIP64 end record and the rest of the directory.
pub(crate) async fn read_entries<R: ReadAt + Send>(
    r: &mut R,
    size: u64,
) -> Result<Vec<Entry>, ReadError<R::Error>> {
    let tail_start = size.saturating_sub(TAIL_LEN);
    let tail = r
        .read_at(tail_start, size - tail_start)
        .await
        .map_err(ReadError::Source)?;
    let dir = match find_end(&tail, tail_start)? {
        End::Found(dir) => dir,
        End::Zip64At(at) => {
            if at.saturating_add(ZIP64_END_LEN as u64) > size {
                return Err(ZipError::Damaged.into());
            }
            let record = r
                .read_at(at, ZIP64_END_LEN as u64)
                .await
                .map_err(ReadError::Source)?;
            zip64_end(&record, at)?
        }
    };
    if dir.len > MAX_DIRECTORY {
        return Err(ZipError::TooBig.into());
    }
    let dir_end = dir.offset + dir.len;
    if dir_end > size {
        return Err(ZipError::Damaged.into());
    }
    // Only the part of the directory the tail doesn't hold.
    let buf = if dir.offset >= tail_start {
        tail[(dir.offset - tail_start) as usize..(dir_end - tail_start) as usize].to_vec()
    } else {
        let head_len = dir_end.min(tail_start) - dir.offset;
        let mut buf = r
            .read_at(dir.offset, head_len)
            .await
            .map_err(ReadError::Source)?;
        if dir_end > tail_start {
            buf.extend_from_slice(&tail[..(dir_end - tail_start) as usize]);
        }
        buf
    };
    Ok(parse_directory(&buf, &dir)?)
}

/// Unpacks one entry's data as it arrives, and checks its size and CRC-32 at the end.
pub(crate) struct Unpacker {
    inflate: Option<flate2::Decompress>,
    ended: bool,
    crc: crc32fast::Hasher,
    written: u64,
    size: u64,
    crc32: u32,
}

impl Unpacker {
    /// For an entry [`Entry::can_unpack`] accepts.
    pub(crate) fn new(entry: &Entry) -> Self {
        Unpacker {
            inflate: (entry.method == DEFLATED).then(|| flate2::Decompress::new(false)),
            ended: false,
            crc: crc32fast::Hasher::new(),
            written: 0,
            size: entry.size,
            crc32: entry.crc32,
        }
    }

    /// Unpacks from `input` into `out`; returns the input bytes used and the bytes written.
    /// Call it again with the rest of the input until it is used up and `out` isn't filled.
    pub(crate) fn step(
        &mut self,
        input: &[u8],
        out: &mut [u8],
    ) -> Result<(usize, usize), ZipError> {
        let (used, written) = match &mut self.inflate {
            None => {
                let n = input.len().min(out.len());
                out[..n].copy_from_slice(&input[..n]);
                (n, n)
            }
            // Whatever follows the end of the deflate stream is not part of the file.
            Some(_) if self.ended => (input.len(), 0),
            Some(d) => {
                let (before_in, before_out) = (d.total_in(), d.total_out());
                let status = d
                    .decompress(input, out, flate2::FlushDecompress::None)
                    .map_err(|_| ZipError::Damaged)?;
                self.ended = status == flate2::Status::StreamEnd;
                let used = (d.total_in() - before_in) as usize;
                let written = (d.total_out() - before_out) as usize;
                if used == 0 && written == 0 && !input.is_empty() && !self.ended {
                    return Err(ZipError::Damaged);
                }
                (used, written)
            }
        };
        self.crc.update(&out[..written]);
        self.written += written as u64;
        // More than the directory promised: stop before it fills the disk.
        if self.written > self.size {
            return Err(ZipError::Damaged);
        }
        Ok((used, written))
    }

    /// Unpacked bytes so far.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// After the last input: the file is complete and unchanged.
    pub(crate) fn finish(self) -> Result<(), ZipError> {
        // An empty file may have no deflate stream at all.
        let complete = self.inflate.is_none() || self.ended || self.size == 0;
        if complete && self.written == self.size && self.crc.finalize() == self.crc32 {
            Ok(())
        } else {
            Err(ZipError::Damaged)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// An archive made by the `zip` crate.
    fn archive(files: &[(&str, &[u8])], comment: &str, method: zip::CompressionMethod) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(method);
        for (name, data) in files {
            if name.ends_with('/') {
                w.add_directory(*name, opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.set_comment(comment).unwrap();
        w.finish().unwrap().into_inner()
    }

    /// Bytes in memory, counting what is read.
    struct Memory {
        bytes: Vec<u8>,
        read: u64,
        requests: usize,
    }

    impl ReadAt for Memory {
        type Error = ();
        async fn read_at(&mut self, from: u64, len: u64) -> Result<Vec<u8>, ()> {
            self.read += len;
            self.requests += 1;
            self.bytes
                .get(from as usize..(from + len) as usize)
                .map(<[u8]>::to_vec)
                .ok_or(())
        }
    }

    fn list(bytes: Vec<u8>) -> (Result<Vec<Entry>, ReadError<()>>, Memory) {
        let size = bytes.len() as u64;
        let mut m = Memory {
            bytes,
            read: 0,
            requests: 0,
        };
        let r = futures_util::FutureExt::now_or_never(read_entries(&mut m, size))
            .expect("in memory, nothing waits");
        (r, m)
    }

    fn unpack(bytes: &[u8], e: &Entry) -> Result<Vec<u8>, ZipError> {
        let start = data_offset(&bytes[e.header as usize..], e)? as usize;
        let data = &bytes[start..start + e.compressed as usize];
        let mut u = Unpacker::new(e);
        let (mut out, mut buf) = (Vec::new(), [0u8; 7]);
        // Small pieces in and out, as from the network.
        for piece in data.chunks(5) {
            let mut rest = piece;
            loop {
                let (used, n) = u.step(rest, &mut buf)?;
                rest = &rest[used..];
                out.extend_from_slice(&buf[..n]);
                if rest.is_empty() && n < buf.len() {
                    break;
                }
            }
        }
        u.finish()?;
        Ok(out)
    }

    #[test]
    fn reads_a_normal_archive() {
        let text = b"hello hello hello hello, what's inside?".repeat(40);
        let bytes = archive(
            &[
                ("readme.txt", &text),
                ("docs/", b""),
                ("docs/guide.md", b"# Guide"),
                ("empty.bin", b""),
            ],
            "",
            zip::CompressionMethod::Deflated,
        );
        let (entries, m) = list(bytes.clone());
        let entries = entries.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["readme.txt", "docs/", "docs/guide.md", "empty.bin"]);
        assert!(entries[1].is_dir());
        assert_eq!(entries[0].size, text.len() as u64);
        assert!(entries[0].compressed < entries[0].size);
        assert_eq!(entries[2].file_name(), "guide.md");
        assert!(entries.iter().all(Entry::can_unpack));
        // A small archive: the end read at once holds the directory too.
        assert_eq!(m.requests, 1);
        assert_eq!(unpack(&bytes, &entries[0]).unwrap(), text);
        assert_eq!(unpack(&bytes, &entries[2]).unwrap(), b"# Guide");
        assert_eq!(unpack(&bytes, &entries[3]).unwrap(), b"");
    }

    #[test]
    fn stored_files_and_a_comment_at_the_end() {
        // The comment contains an end record signature: it isn't taken for the real one.
        let comment = format!("made by a test PK\u{5}\u{6}{}", "x".repeat(300));
        let bytes = archive(
            &[("a.txt", b"stored as is")],
            &comment,
            zip::CompressionMethod::Stored,
        );
        let (entries, _) = list(bytes.clone());
        let entries = entries.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].method, STORED);
        assert_eq!(unpack(&bytes, &entries[0]).unwrap(), b"stored as is");
    }

    #[test]
    fn reads_only_the_end_of_a_big_archive() {
        // 2 MB of incompressible data, then 300 small files.
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let noise: Vec<u8> = (0..2 << 20)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng as u8
            })
            .collect();
        let mut files: Vec<(String, Vec<u8>)> = vec![("big.bin".into(), noise)];
        for i in 0..300 {
            files.push((
                format!("folder/file-{i:03}.txt"),
                format!("{i}").into_bytes(),
            ));
        }
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        let bytes = archive(&refs, "", zip::CompressionMethod::Deflated);
        let (entries, m) = list(bytes.clone());
        let entries = entries.unwrap();
        assert_eq!(entries.len(), 301);
        assert!(m.read < 100_000, "read {} bytes", m.read);
        assert_eq!(unpack(&bytes, &entries[300]).unwrap(), b"299");
        assert_eq!(unpack(&bytes, &entries[0]).unwrap(), files[0].1);
    }

    #[test]
    fn a_directory_beyond_the_tail_is_read_in_one_more_request() {
        let names: Vec<String> = (0..1500)
            .map(|i| format!("a/rather/long/path/to/make/the/directory/big/file-{i:05}.txt"))
            .collect();
        let refs: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let bytes = archive(&refs, "", zip::CompressionMethod::Stored);
        let (entries, m) = list(bytes);
        assert_eq!(entries.unwrap().len(), 1500);
        assert_eq!(m.requests, 2);
    }

    /// A ZIP64 archive written by hand: sizes, offsets and counts in the ZIP64 records.
    fn zip64_archive(prefix: usize) -> Vec<u8> {
        let data = b"zip64 data";
        let crc = crc32fast::hash(data);
        let name = b"big/file.dat";
        let mut out = vec![0u8; prefix];
        let local_at = 0u64;
        // Local header with a ZIP64 extra field.
        out.extend(SIG_LOCAL.to_le_bytes());
        out.extend([45, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend(crc.to_le_bytes());
        out.extend(u32::MAX.to_le_bytes());
        out.extend(u32::MAX.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(20u16.to_le_bytes());
        out.extend(name);
        out.extend(1u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend((data.len() as u64).to_le_bytes());
        out.extend((data.len() as u64).to_le_bytes());
        out.extend(data);
        let dir_at = (out.len() - prefix) as u64;
        out.extend(SIG_CENTRAL.to_le_bytes());
        out.extend([45, 3, 45, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend(crc.to_le_bytes());
        out.extend(u32::MAX.to_le_bytes());
        out.extend(u32::MAX.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(28u16.to_le_bytes());
        // Comment length, disk, internal and external attributes.
        out.extend([0u8; 10]);
        out.extend(u32::MAX.to_le_bytes());
        out.extend(name);
        out.extend(1u16.to_le_bytes());
        out.extend(24u16.to_le_bytes());
        out.extend((data.len() as u64).to_le_bytes());
        out.extend((data.len() as u64).to_le_bytes());
        out.extend(local_at.to_le_bytes());
        let dir_len = (out.len() - prefix) as u64 - dir_at;
        let record_at = (out.len() - prefix) as u64;
        out.extend(SIG_ZIP64_END.to_le_bytes());
        out.extend(44u64.to_le_bytes());
        out.extend([45, 3, 45, 0]);
        out.extend([0u8; 8]);
        out.extend(1u64.to_le_bytes());
        out.extend(1u64.to_le_bytes());
        out.extend(dir_len.to_le_bytes());
        out.extend(dir_at.to_le_bytes());
        out.extend(SIG_ZIP64_LOCATOR.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(record_at.to_le_bytes());
        out.extend(1u32.to_le_bytes());
        out.extend(SIG_END.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend([0xff; 4]);
        out.extend([0xff; 8]);
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn reads_zip64_records() {
        let bytes = zip64_archive(0);
        let (entries, _) = list(bytes.clone());
        let entries = entries.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "big/file.dat");
        assert_eq!(entries[0].size, 10);
        assert_eq!(entries[0].header, 0);
        assert_eq!(unpack(&bytes, &entries[0]).unwrap(), b"zip64 data");
    }

    #[test]
    fn a_self_extracting_program_in_front_shifts_the_offsets() {
        let bytes = zip64_archive(1000);
        let (entries, _) = list(bytes.clone());
        let entries = entries.unwrap();
        assert_eq!(entries[0].header, 1000);
        assert_eq!(unpack(&bytes, &entries[0]).unwrap(), b"zip64 data");

        let mut sfx = vec![b'M', b'Z'];
        sfx.resize(5000, 0);
        sfx.extend(archive(
            &[("setup.ini", b"[setup]")],
            "",
            zip::CompressionMethod::Deflated,
        ));
        let (entries, _) = list(sfx.clone());
        let entries = entries.unwrap();
        assert_eq!(entries[0].header, 5000);
        assert_eq!(unpack(&sfx, &entries[0]).unwrap(), b"[setup]");
    }

    #[test]
    fn the_zip_crate_s_zip64_entries_are_read() {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().large_file(true);
        w.start_file("large.txt", opts).unwrap();
        w.write_all(b"large file flag").unwrap();
        let bytes = w.finish().unwrap().into_inner();
        let (entries, _) = list(bytes.clone());
        let entries = entries.unwrap();
        assert_eq!(entries[0].size, 15);
        assert_eq!(unpack(&bytes, &entries[0]).unwrap(), b"large file flag");
    }

    #[test]
    fn garbage_and_cut_archives_are_errors_not_panics() {
        let (r, _) = list(Vec::new());
        assert_eq!(r.unwrap_err(), ReadError::Zip(ZipError::NotZip));
        let (r, _) = list(b"<html>Not found</html>".repeat(50));
        assert_eq!(r.unwrap_err(), ReadError::Zip(ZipError::NotZip));

        let good = archive(
            &[("a.txt", b"aaaa"), ("b.txt", b"bbbb")],
            "",
            zip::CompressionMethod::Deflated,
        );
        // Every cut: an error or fewer files, never a panic.
        for cut in 0..good.len() {
            let _ = list(good[..cut].to_vec());
            let _ = list(good[cut..].to_vec());
        }
        // The directory cut short.
        let end = good.len() - END_LEN;
        let dir = find_end(&good[end..], end as u64).unwrap();
        let End::Found(dir) = dir else { panic!() };
        let cd = &good[dir.offset as usize..(dir.offset + dir.len) as usize];
        assert_eq!(
            parse_directory(&cd[..cd.len() - 3], &dir),
            Err(ZipError::Damaged)
        );
        // Offsets pointing past the end record.
        let mut bad = good.clone();
        let at = bad.len() - 6;
        bad[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(list(bad).0.unwrap_err(), ReadError::Zip(ZipError::Damaged));
        // Random bytes.
        let mut rng = 7u32;
        let noise: Vec<u8> = (0..70_000)
            .map(|_| {
                rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (rng >> 16) as u8
            })
            .collect();
        assert!(list(noise).0.is_err());
    }

    #[test]
    fn damaged_data_is_caught() {
        let text = b"some text to deflate ".repeat(20);
        let bytes = archive(&[("t.txt", &text)], "", zip::CompressionMethod::Deflated);
        let (entries, _) = list(bytes.clone());
        let e = &entries.unwrap()[0];
        // A flipped byte in the data: the CRC (or the deflate stream) doesn't match.
        let start = data_offset(&bytes[e.header as usize..], e).unwrap() as usize;
        let mut bad = bytes.clone();
        bad[start + 3] ^= 0x55;
        assert_eq!(unpack(&bad, e), Err(ZipError::Damaged));
        // The directory claims a smaller file than the data holds.
        let small = Entry {
            size: 10,
            ..e.clone()
        };
        assert_eq!(unpack(&bytes, &small), Err(ZipError::Damaged));
        // A local header that isn't one.
        assert_eq!(
            data_offset(b"garbage garbage garbage garbage", e),
            Err(ZipError::Damaged)
        );
    }

    #[test]
    fn names_in_other_encodings() {
        assert_eq!(CP437_HIGH.chars().count(), 128);
        // "Prüfung.txt" in code page 437, without the UTF-8 flag.
        assert_eq!(entry_name(b"Pr\x81fung.txt", 0, b""), "Prüfung.txt");
        assert_eq!(
            entry_name("Žluťoučký kůň.txt".as_bytes(), 0x800, b""),
            "Žluťoučký kůň.txt"
        );
        assert_eq!(
            entry_name(b"dir\\sub\\file.txt", 0, b""),
            "dir/sub/file.txt"
        );
        // The Info-ZIP Unicode Path field wins when it belongs to the name.
        let raw = b"caf\x82.txt";
        let mut extra = vec![0x75, 0x70];
        let unicode = "café.txt".as_bytes();
        extra.extend(((5 + unicode.len()) as u16).to_le_bytes());
        extra.push(1);
        extra.extend(crc32fast::hash(raw).to_le_bytes());
        extra.extend(unicode);
        assert_eq!(entry_name(raw, 0, &extra), "café.txt");
    }

    #[test]
    fn encrypted_and_other_methods_cannot_be_unpacked() {
        let e = Entry {
            name: "secret.txt".into(),
            method: DEFLATED,
            flags: 1,
            crc32: 0,
            compressed: 10,
            size: 10,
            header: 0,
        };
        assert!(e.encrypted() && !e.can_unpack());
        let aes = Entry {
            method: AES,
            flags: 0,
            ..e.clone()
        };
        assert!(aes.encrypted() && !aes.can_unpack());
        let bzip2 = Entry {
            method: 12,
            flags: 0,
            ..e
        };
        assert!(!bzip2.encrypted() && !bzip2.can_unpack());
    }
}
