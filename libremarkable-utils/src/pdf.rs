//! A minimal, conservative PDF page counter: bytes in, a page count out,
//! no I/O and no native dependencies. `rmu hub add` uses it to write a
//! materialised page list (see "History, restore and add" in
//! `docs/hub-design.md`).
//!
//! It finds the cross-reference data from `startxref`: classic tables
//! and cross-reference streams, hybrid files (`/XRefStm`), incremental
//! updates (`/Prev`), and objects inside object streams. Flate streams
//! (with PNG predictors) are decoded with `flate2`. If that data is
//! unusable, the table is rebuilt by scanning for `N G obj`. The count
//! is `/Root` → `/Pages` → `/Count`, and it is only returned when a walk
//! of the page tree finds exactly that many pages: anything doubtful
//! (an encrypted object stream, a filter other than Flate, a damaged or
//! cyclic tree) is `None`, never a guess. Every loop and allocation is
//! bounded, so a hostile file cannot make it run away.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::rc::Rc;

/// Nesting of arrays and dictionaries the parser follows.
const MAX_NESTING: usize = 64;
/// Cross-reference sections followed through `/Prev` and `/XRefStm`.
const MAX_SECTIONS: usize = 256;
/// Bytes a decoded stream may grow to.
const MAX_DECODED: u64 = 256 * 1024 * 1024;
/// Hops through indirect references to reach a value.
const MAX_HOPS: usize = 32;
/// Nodes of the page tree visited.
const MAX_NODES: usize = 4_000_000;
/// Depth of the page tree.
const MAX_DEPTH: usize = 256;

/// The number of pages of a PDF, if it can be read reliably (see the
/// module docs); `None` otherwise.
pub fn page_count(pdf: &[u8]) -> Option<usize> {
    let header = find(pdf, b"%PDF-", 0).filter(|&at| at < 1024)?;
    let from_xref = startxref(pdf)
        .and_then(|offset| load_xref(pdf, offset, header))
        .and_then(|xref| Resolver::new(pdf, xref).page_count());
    from_xref.or_else(|| Resolver::new(pdf, scan(pdf)?).page_count())
}

// ---- objects ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Obj {
    Null,
    Bool(bool),
    Int(i64),
    Real,
    Str,
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
}

type Dict = Vec<(Vec<u8>, Obj)>;

fn get<'d>(dict: &'d Dict, key: &[u8]) -> Option<&'d Obj> {
    dict.iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

impl Obj {
    fn int(&self) -> Option<i64> {
        match self {
            Obj::Int(value) => Some(*value),
            _ => None,
        }
    }

    fn name(&self) -> Option<&[u8]> {
        match self {
            Obj::Name(name) => Some(name),
            _ => None,
        }
    }
}

/// An indirect object as found at an offset: its value, and the raw
/// bytes of its stream if it is one.
struct Indirect<'a> {
    number: u32,
    value: Obj,
    stream: Option<&'a [u8]>,
}

// ---- lexing and parsing -------------------------------------------------------

fn is_white(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn is_regular(byte: u8) -> bool {
    !is_white(byte) && !is_delimiter(byte)
}

fn find(data: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

fn rfind(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .rposition(|window| window == needle)
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn at(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos: pos.min(data.len()),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn skip_white(&mut self) {
        while let Some(byte) = self.peek() {
            if is_white(byte) {
                self.pos += 1;
            } else if byte == b'%' {
                while self.peek().is_some_and(|b| b != b'\n' && b != b'\r') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    /// The run of regular characters at the position.
    fn word(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self.peek().is_some_and(is_regular) {
            self.pos += 1;
        }
        &self.data[start..self.pos]
    }

    /// The keyword `word` next (after white space), consumed if so.
    fn keyword(&mut self, word: &[u8]) -> bool {
        self.skip_white();
        let save = self.pos;
        if self.word() == word {
            true
        } else {
            self.pos = save;
            false
        }
    }

    /// A non-negative integer next, consumed if so.
    fn unsigned(&mut self) -> Option<u64> {
        self.skip_white();
        let save = self.pos;
        let word = self.word();
        match std::str::from_utf8(word).ok().and_then(|w| w.parse().ok()) {
            Some(value) if word.iter().all(u8::is_ascii_digit) => Some(value),
            _ => {
                self.pos = save;
                None
            }
        }
    }

    fn object(&mut self, depth: usize) -> Option<Obj> {
        if depth > MAX_NESTING {
            return None;
        }
        self.skip_white();
        match self.peek()? {
            b'/' => {
                self.pos += 1;
                Some(Obj::Name(decode_name(self.word())))
            }
            b'<' if self.data.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                self.dict_body(depth).map(Obj::Dict)
            }
            b'<' => {
                let end = find(self.data, b">", self.pos)?;
                self.pos = end + 1;
                Some(Obj::Str)
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_white();
                    if self.peek()? == b']' {
                        self.pos += 1;
                        return Some(Obj::Array(items));
                    }
                    items.push(self.object(depth + 1)?);
                }
            }
            b'(' => self.literal_string().map(|()| Obj::Str),
            b'0'..=b'9' | b'+' | b'-' | b'.' => self.number_or_ref(),
            _ => match self.word() {
                b"true" => Some(Obj::Bool(true)),
                b"false" => Some(Obj::Bool(false)),
                b"null" => Some(Obj::Null),
                _ => None,
            },
        }
    }

    /// After `<<`: key/value pairs up to `>>`.
    fn dict_body(&mut self, depth: usize) -> Option<Dict> {
        let mut dict = Dict::new();
        loop {
            self.skip_white();
            match self.peek()? {
                b'>' if self.data.get(self.pos + 1) == Some(&b'>') => {
                    self.pos += 2;
                    return Some(dict);
                }
                b'/' => {
                    self.pos += 1;
                    let key = decode_name(self.word());
                    let value = self.object(depth + 1)?;
                    dict.push((key, value));
                }
                _ => return None,
            }
        }
    }

    fn literal_string(&mut self) -> Option<()> {
        self.pos += 1;
        let mut open = 1usize;
        while open > 0 {
            match self.peek()? {
                b'\\' => self.pos += 1,
                b'(' => open += 1,
                b')' => open -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        Some(())
    }

    /// A number, or `N G R` when an integer is followed by one and `R`.
    fn number_or_ref(&mut self) -> Option<Obj> {
        let word = self.word();
        let text = std::str::from_utf8(word).ok()?;
        let Ok(value) = text.parse::<i64>() else {
            return text.parse::<f64>().ok().map(|_| Obj::Real);
        };
        let save = self.pos;
        let reference = (|| {
            let number = u32::try_from(value).ok()?;
            self.unsigned()?;
            self.keyword(b"R").then_some(Obj::Ref(number))
        })();
        if reference.is_none() {
            self.pos = save;
        }
        Some(reference.unwrap_or(Obj::Int(value)))
    }

    /// `N G obj <value> [stream ... endstream]` at the position.
    fn indirect(&mut self) -> Option<Indirect<'a>> {
        let number = u32::try_from(self.unsigned()?).ok()?;
        self.unsigned()?;
        if !self.keyword(b"obj") {
            return None;
        }
        let value = self.object(0)?;
        let stream = match &value {
            Obj::Dict(dict) if self.keyword(b"stream") => Some(self.stream_data(dict)?),
            _ => None,
        };
        Some(Indirect {
            number,
            value,
            stream,
        })
    }

    /// After `stream`: the raw data, by a direct `/Length` that ends at
    /// `endstream`, else up to the next `endstream`.
    fn stream_data(&mut self, dict: &Dict) -> Option<&'a [u8]> {
        match self.data.get(self.pos..self.pos + 2)? {
            b"\r\n" => self.pos += 2,
            [b'\n', _] | [b'\r', _] => self.pos += 1,
            _ => {}
        }
        let start = self.pos;
        let by_length = get(dict, b"Length")
            .and_then(Obj::int)
            .and_then(|length| usize::try_from(length).ok())
            .and_then(|length| start.checked_add(length))
            .filter(|&end| {
                let mut after = Parser::at(self.data, end);
                end <= self.data.len() && after.keyword(b"endstream")
            });
        let end = match by_length {
            Some(end) => end,
            None => {
                let mut end = find(self.data, b"endstream", start)?;
                while end > start && matches!(self.data[end - 1], b'\n' | b'\r') {
                    end -= 1;
                }
                end
            }
        };
        self.pos = end;
        Some(&self.data[start..end])
    }
}

/// A name's `#xx` escapes resolved.
fn decode_name(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        let escaped = (raw[at] == b'#')
            .then(|| raw.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(raw[at]);
                at += 1;
            }
        }
    }
    out
}

// ---- streams ------------------------------------------------------------------

/// A stream's decoded data: unfiltered, or Flate with an optional PNG
/// predictor. Any other filter is `None`.
fn decode_stream(dict: &Dict, raw: &[u8]) -> Option<Vec<u8>> {
    let filters: Vec<&[u8]> = match get(dict, b"Filter") {
        None | Some(Obj::Null) => Vec::new(),
        Some(Obj::Name(name)) => vec![name.as_slice()],
        Some(Obj::Array(items)) => items.iter().map(Obj::name).collect::<Option<_>>()?,
        Some(_) => return None,
    };
    let params = match get(dict, b"DecodeParms").or_else(|| get(dict, b"DP")) {
        Some(Obj::Dict(params)) => Some(params),
        Some(Obj::Array(items)) => match items.as_slice() {
            [Obj::Dict(params)] => Some(params),
            _ => None,
        },
        _ => None,
    };
    match filters.as_slice() {
        [] => Some(raw.to_vec()),
        [b"FlateDecode" | b"Fl"] => unpredict(&inflate(raw)?, params),
        _ => None,
    }
}

fn inflate(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut reader = flate2::read::ZlibDecoder::new(raw).take(MAX_DECODED + 1);
    // A truncated or checksum-damaged stream still yields what it
    // decoded; the page-tree walk judges whether it made sense.
    let read = reader.read_to_end(&mut out);
    if out.len() as u64 > MAX_DECODED || (read.is_err() && out.is_empty()) {
        return None;
    }
    Some(out)
}

/// Undo a PNG predictor (`/Predictor` 10 to 15); none (1) is a no-op,
/// TIFF (2) is unsupported.
fn unpredict(data: &[u8], params: Option<&Dict>) -> Option<Vec<u8>> {
    let param = |key: &[u8], default: i64| {
        params
            .and_then(|params| get(params, key))
            .and_then(Obj::int)
            .unwrap_or(default)
    };
    let predictor = param(b"Predictor", 1);
    if predictor == 1 {
        return Some(data.to_vec());
    }
    if !(10..=15).contains(&predictor) {
        return None;
    }
    let bits = usize::try_from(param(b"Colors", 1) * param(b"BitsPerComponent", 8)).ok()?;
    let columns = usize::try_from(param(b"Columns", 1)).ok()?;
    let bpp = (bits / 8).max(1);
    let row = bits.checked_mul(columns)?.div_ceil(8);
    if row == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut previous = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        let (&filter, bytes) = chunk.split_first()?;
        let mut current = bytes.to_vec();
        current.resize(row, 0);
        (0..row).for_each(|i| {
            let left = if i >= bpp { current[i - bpp] } else { 0 };
            let up = previous[i];
            let upper_left = if i >= bpp { previous[i - bpp] } else { 0 };
            let add = match filter {
                1 => left,
                2 => up,
                3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                4 => paeth(left, up, upper_left),
                _ => 0,
            };
            current[i] = current[i].wrapping_add(add);
        });
        if filter > 4 {
            return None;
        }
        out.extend_from_slice(&current[..bytes.len().min(row)]);
        previous = current;
    }
    Some(out)
}

fn paeth(left: u8, up: u8, upper_left: u8) -> u8 {
    let p = i16::from(left) + i16::from(up) - i16::from(upper_left);
    let (pa, pb, pc) = (
        (p - i16::from(left)).abs(),
        (p - i16::from(up)).abs(),
        (p - i16::from(upper_left)).abs(),
    );
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        upper_left
    }
}

// ---- cross-reference data -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// At a byte offset.
    Offset(usize),
    /// Inside object stream `stream`, at `index`.
    InStream { stream: u32, index: usize },
}

#[derive(Debug, Default)]
struct Xref {
    entries: HashMap<u32, Entry>,
    /// The newest trailer's `/Root`.
    root: Option<Obj>,
}

/// The offset `startxref` names.
fn startxref(pdf: &[u8]) -> Option<usize> {
    let at = rfind(pdf, b"startxref")?;
    let mut parser = Parser::at(pdf, at + b"startxref".len());
    usize::try_from(parser.unsigned()?).ok()
}

/// Every cross-reference section from `offset` on, newest first (an
/// entry a newer section has wins). Offsets are tried as written, then
/// relative to the header, for files with bytes before `%PDF-`.
fn load_xref(pdf: &[u8], offset: usize, header: usize) -> Option<Xref> {
    let mut xref = Xref::default();
    let mut pending = vec![offset];
    let mut seen = HashSet::new();
    while let Some(offset) = pending.pop() {
        if seen.len() >= MAX_SECTIONS {
            return None;
        }
        if !seen.insert(offset) {
            continue;
        }
        let section = section(pdf, offset).or_else(|| {
            (header > 0)
                .then(|| section(pdf, offset.checked_add(header)?))
                .flatten()
        })?;
        section.entries.into_iter().for_each(|(number, entry)| {
            xref.entries.entry(number).or_insert(entry);
        });
        if xref.root.is_none() {
            xref.root = get(&section.trailer, b"Root").cloned();
        }
        // `/Prev` after `/XRefStm`: the stack pops the hybrid stream
        // first, so its entries rank with its own section's.
        let next = |key: &[u8]| {
            get(&section.trailer, key)
                .and_then(Obj::int)
                .and_then(|value| usize::try_from(value).ok())
        };
        pending.extend(next(b"Prev"));
        pending.extend(next(b"XRefStm"));
    }
    xref.root.is_some().then_some(xref)
}

struct Section {
    entries: Vec<(u32, Entry)>,
    trailer: Dict,
}

/// One section at `offset`: a classic table (`xref`) with its trailer,
/// or a cross-reference stream.
fn section(pdf: &[u8], offset: usize) -> Option<Section> {
    let mut parser = Parser::at(pdf, offset);
    if parser.keyword(b"xref") {
        return classic(&mut parser);
    }
    let mut parser = Parser::at(pdf, offset);
    let object = parser.indirect()?;
    let Obj::Dict(dict) = object.value else {
        return None;
    };
    if get(&dict, b"Type").and_then(Obj::name) != Some(b"XRef") {
        return None;
    }
    let data = decode_stream(&dict, object.stream?)?;
    Some(Section {
        entries: stream_entries(&dict, &data)?,
        trailer: dict,
    })
}

/// A classic table after `xref`: subsections of `start count` and
/// `count` entries `offset generation n|f`, then `trailer << ... >>`.
/// Free entries are left out (a missing entry is free).
fn classic(parser: &mut Parser) -> Option<Section> {
    let mut entries = Vec::new();
    loop {
        if parser.keyword(b"trailer") {
            parser.skip_white();
            let Obj::Dict(trailer) = parser.object(0)? else {
                return None;
            };
            return Some(Section { entries, trailer });
        }
        let start = u32::try_from(parser.unsigned()?).ok()?;
        let count = u32::try_from(parser.unsigned()?).ok()?;
        for number in start..start.checked_add(count)? {
            let offset = usize::try_from(parser.unsigned()?).ok()?;
            parser.unsigned()?;
            parser.skip_white();
            match parser.word() {
                b"n" if offset > 0 => entries.push((number, Entry::Offset(offset))),
                b"n" | b"f" => {}
                _ => return None,
            }
        }
    }
}

/// The entries of a decoded cross-reference stream (`/W`, `/Index`).
fn stream_entries(dict: &Dict, data: &[u8]) -> Option<Vec<(u32, Entry)>> {
    let ints = |key: &[u8]| -> Option<Vec<i64>> {
        match get(dict, key)? {
            Obj::Array(items) => items.iter().map(Obj::int).collect(),
            _ => None,
        }
    };
    let widths: Vec<usize> = ints(b"W")?
        .into_iter()
        .map(|w| usize::try_from(w).ok().filter(|&w| w <= 8))
        .collect::<Option<_>>()?;
    let [w0, w1, w2] = widths.as_slice() else {
        return None;
    };
    let record = w0 + w1 + w2;
    if record == 0 {
        return None;
    }
    let size = get(dict, b"Size").and_then(Obj::int)?;
    let index = ints(b"Index").unwrap_or_else(|| vec![0, size]);
    let ranges: Vec<(u32, u32)> = index
        .chunks(2)
        .map(|pair| match pair {
            [start, count] => Some((u32::try_from(*start).ok()?, u32::try_from(*count).ok()?)),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let field = |bytes: &[u8]| bytes.iter().fold(0u64, |v, &b| (v << 8) | u64::from(b));
    let mut records = data.chunks_exact(record);
    let mut entries = Vec::new();
    for (start, count) in ranges {
        for number in start..start.checked_add(count)? {
            let bytes = records.next()?;
            let kind = if *w0 == 0 { 1 } else { field(&bytes[..*w0]) };
            let second = field(&bytes[*w0..w0 + w1]);
            let third = field(&bytes[w0 + w1..]);
            match kind {
                1 if second > 0 => {
                    entries.push((number, Entry::Offset(usize::try_from(second).ok()?)))
                }
                2 => entries.push((
                    number,
                    Entry::InStream {
                        stream: u32::try_from(second).ok()?,
                        index: usize::try_from(third).ok()?,
                    },
                )),
                _ => {}
            }
        }
    }
    Some(entries)
}

/// The table rebuilt by scanning for `N G obj` (the last definition of a
/// number wins, as an incremental update's would), plus the objects of
/// every object stream not defined directly; `/Root` from the last
/// trailer or cross-reference stream that has one, else the last
/// `/Type /Catalog`.
fn scan(pdf: &[u8]) -> Option<Xref> {
    let mut entries = HashMap::new();
    let mut at = 0;
    while let Some(found) = find(pdf, b"obj", at) {
        at = found + 3;
        if pdf.get(at).is_some_and(|&b| is_regular(b)) {
            continue;
        }
        if let Some((number, start)) = object_header_before(pdf, found) {
            entries.insert(number, Entry::Offset(start));
        }
    }
    let direct: Vec<(u32, usize)> = entries
        .iter()
        .filter_map(|(&number, entry)| match entry {
            Entry::Offset(offset) => Some((number, *offset)),
            Entry::InStream { .. } => None,
        })
        .collect();
    let mut root = None;
    let mut catalog = None;
    let mut sorted = direct.clone();
    sorted.sort_by_key(|&(_, offset)| offset);
    for &(number, offset) in &sorted {
        let Some(object) = Parser::at(pdf, offset).indirect() else {
            continue;
        };
        let Obj::Dict(dict) = &object.value else {
            continue;
        };
        match get(dict, b"Type").and_then(Obj::name) {
            Some(b"Catalog") => catalog = Some(Obj::Ref(number)),
            Some(b"XRef") => root = get(dict, b"Root").cloned().or(root),
            Some(b"ObjStm") => {
                let members = object
                    .stream
                    .and_then(|raw| decode_stream(dict, raw))
                    .and_then(|data| object_stream_header(dict, &data));
                members
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .for_each(|(index, (member, _))| {
                        entries.entry(member).or_insert(Entry::InStream {
                            stream: number,
                            index,
                        });
                    });
            }
            _ => {}
        }
    }
    let mut trailer_at = 0;
    while let Some(found) = find(pdf, b"trailer", trailer_at) {
        trailer_at = found + 7;
        if let Some(Obj::Dict(dict)) = Parser::at(pdf, trailer_at).object(0)
            && let Some(found) = get(&dict, b"Root")
        {
            root = Some(found.clone());
        }
    }
    Some(Xref {
        entries,
        root: Some(root.or(catalog)?),
    })
}

/// The object number and start of an `N G obj` header ending right
/// before `obj` at `at`.
fn object_header_before(pdf: &[u8], at: usize) -> Option<(u32, usize)> {
    let digits_before = |end: usize| -> Option<usize> {
        let start = pdf[..end]
            .iter()
            .rposition(|b| !b.is_ascii_digit())
            .map_or(0, |p| p + 1);
        (start < end).then_some(start)
    };
    let white_before = |end: usize| -> Option<usize> {
        let start = pdf[..end]
            .iter()
            .rposition(|&b| !is_white(b))
            .map_or(0, |p| p + 1);
        (start < end).then_some(start)
    };
    let generation = digits_before(white_before(at)?)?;
    let number = digits_before(white_before(generation)?)?;
    if number > 0 && is_regular(pdf[number - 1]) {
        return None;
    }
    let text = std::str::from_utf8(&pdf[number..white_before(generation)?]).ok()?;
    Some((text.parse().ok()?, number))
}

/// An object stream's header: `/N` pairs of object number and offset
/// (relative to `/First`).
fn object_stream_header(dict: &Dict, data: &[u8]) -> Option<Vec<(u32, usize)>> {
    let count = usize::try_from(get(dict, b"N").and_then(Obj::int)?).ok()?;
    let first = usize::try_from(get(dict, b"First").and_then(Obj::int)?).ok()?;
    let mut parser = Parser::at(data.get(..first)?, 0);
    (0..count)
        .map(|_| {
            let number = u32::try_from(parser.unsigned()?).ok()?;
            let offset = usize::try_from(parser.unsigned()?).ok()?;
            Some((number, first.checked_add(offset)?))
        })
        .collect()
}

// ---- resolving and counting ---------------------------------------------------

struct ObjectStream {
    data: Vec<u8>,
    members: Vec<(u32, usize)>,
}

struct Resolver<'a> {
    pdf: &'a [u8],
    xref: Xref,
    streams: RefCell<HashMap<u32, Option<Rc<ObjectStream>>>>,
}

impl<'a> Resolver<'a> {
    fn new(pdf: &'a [u8], xref: Xref) -> Self {
        Self {
            pdf,
            xref,
            streams: RefCell::new(HashMap::new()),
        }
    }

    /// The value of object `number`.
    fn object(&self, number: u32) -> Option<Obj> {
        match *self.xref.entries.get(&number)? {
            Entry::Offset(offset) => {
                let object = Parser::at(self.pdf, offset).indirect()?;
                (object.number == number).then_some(object.value)
            }
            Entry::InStream { stream, index } => {
                let stream = self.object_stream(stream)?;
                let &(member, offset) = stream.members.get(index)?;
                if member != number {
                    return None;
                }
                Parser::at(&stream.data, offset).object(0)
            }
        }
    }

    fn object_stream(&self, number: u32) -> Option<Rc<ObjectStream>> {
        if let Some(known) = self.streams.borrow().get(&number) {
            return known.clone();
        }
        let loaded = (|| {
            let Entry::Offset(offset) = *self.xref.entries.get(&number)? else {
                return None;
            };
            let object = Parser::at(self.pdf, offset).indirect()?;
            let Obj::Dict(dict) = &object.value else {
                return None;
            };
            let data = decode_stream(dict, object.stream?)?;
            let members = object_stream_header(dict, &data)?;
            Some(Rc::new(ObjectStream { data, members }))
        })();
        self.streams.borrow_mut().insert(number, loaded.clone());
        loaded
    }

    /// `value` with indirect references followed.
    fn resolve(&self, value: &Obj) -> Option<Obj> {
        let mut current = value.clone();
        for _ in 0..MAX_HOPS {
            match current {
                Obj::Ref(number) => current = self.object(number)?,
                other => return Some(other),
            }
        }
        None
    }

    fn dict(&self, value: &Obj) -> Option<Dict> {
        match self.resolve(value)? {
            Obj::Dict(dict) => Some(dict),
            _ => None,
        }
    }

    /// `/Count` of the page tree's root, if a walk of the tree agrees.
    fn page_count(&self) -> Option<usize> {
        let catalog = self.dict(self.xref.root.as_ref()?)?;
        let root = get(&catalog, b"Pages")?;
        let pages = self.dict(root)?;
        let count = usize::try_from(self.resolve(get(&pages, b"Count")?)?.int()?).ok()?;
        (count > 0 && self.walk(root)? == count).then_some(count)
    }

    /// The leaves (pages) below `root`; `None` on a cycle, a node seen
    /// twice, a node that is neither, or a bound exceeded.
    fn walk(&self, root: &Obj) -> Option<usize> {
        let mut stack = vec![(root.clone(), 0usize)];
        let mut seen = HashSet::new();
        let mut visited = 0usize;
        let mut leaves = 0usize;
        while let Some((node, depth)) = stack.pop() {
            visited += 1;
            if visited > MAX_NODES || depth > MAX_DEPTH {
                return None;
            }
            if let Obj::Ref(number) = node
                && !seen.insert(number)
            {
                return None;
            }
            let dict = self.dict(&node)?;
            let kids = get(&dict, b"Kids");
            match (get(&dict, b"Type").and_then(Obj::name), kids) {
                (Some(b"Pages") | None, Some(kids)) => match self.resolve(kids)? {
                    Obj::Array(kids) => stack.extend(kids.into_iter().map(|kid| (kid, depth + 1))),
                    _ => return None,
                },
                (Some(b"Page") | None, None) => leaves += 1,
                _ => return None,
            }
        }
        Some(leaves)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A PDF of `objects` (numbered from 1) with a classic table and the
    /// given trailer entries (`/Size` added).
    fn classic_pdf(objects: &[String], trailer: &str) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let offsets: Vec<usize> = objects
            .iter()
            .enumerate()
            .map(|(i, body)| {
                let at = out.len();
                out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
                at
            })
            .collect();
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f\r\n");
        offsets
            .iter()
            .for_each(|at| out.extend_from_slice(format!("{at:010} 00000 n\r\n").as_bytes()));
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} {trailer} >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// Catalog (1), page tree root (2) with `pages` leaves (3..).
    fn flat_tree(pages: usize, count: usize) -> Vec<String> {
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
        [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {count} >>",
                kids.join(" ")
            ),
        ]
        .into_iter()
        .chain(
            (0..pages)
                .map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string()),
        )
        .collect()
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn classic_tables() {
        let pdf = classic_pdf(&flat_tree(3, 3), "/Root 1 0 R");
        assert_eq!(page_count(&pdf), Some(3));
        // A nested tree, an indirect /Count, comments and strings in the way.
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R /Title (a (nested) \\) string) >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 7 0 R >>".to_string(),
            "<< /Type /Pages /Parent 2 0 R /Kids [5 0 R 6 0 R] /Count 2 >> % a comment".to_string(),
            "<< /Type /Page /Parent 2 0 R /Contents <0a0b> >>".to_string(),
            "<< /Type /Page /Parent 3 0 R >>".to_string(),
            "<< /Type /Page /Parent 3 0 R >>".to_string(),
            "3".to_string(),
        ];
        assert_eq!(page_count(&classic_pdf(&objects, "/Root 1 0 R")), Some(3));
    }

    #[test]
    fn a_count_the_tree_does_not_confirm_is_refused() {
        assert_eq!(
            page_count(&classic_pdf(&flat_tree(3, 4), "/Root 1 0 R")),
            None
        );
        // A cycle: the page tree root lists itself.
        let mut objects = flat_tree(1, 2);
        objects[1] = "<< /Type /Pages /Kids [3 0 R 2 0 R] /Count 2 >>".to_string();
        assert_eq!(page_count(&classic_pdf(&objects, "/Root 1 0 R")), None);
        // A page listed twice.
        objects[1] = "<< /Type /Pages /Kids [3 0 R 3 0 R] /Count 2 >>".to_string();
        assert_eq!(page_count(&classic_pdf(&objects, "/Root 1 0 R")), None);
        // No pages at all.
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [] /Count 0 >>".to_string(),
        ];
        assert_eq!(page_count(&classic_pdf(&objects, "/Root 1 0 R")), None);
    }

    #[test]
    fn not_a_pdf() {
        assert_eq!(page_count(b""), None);
        assert_eq!(page_count(b"%!PS-Adobe-3.0\n"), None);
        assert_eq!(page_count(b"%PDF-1.7\nnothing here\n"), None);
        // A header too far in.
        let mut late = vec![b' '; 2000];
        late.extend(classic_pdf(&flat_tree(1, 1), "/Root 1 0 R"));
        assert_eq!(page_count(&late), None);
    }

    #[test]
    fn incremental_updates_take_the_newest_objects() {
        let mut pdf = classic_pdf(&flat_tree(2, 2), "/Root 1 0 R");
        let first_xref = startxref(&pdf).unwrap();
        // The update replaces the page tree root with one of three pages
        // (adding object 5) and chains to the first table.
        let page = pdf.len();
        pdf.extend_from_slice(b"5 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n");
        let root = pdf.len();
        pdf.extend_from_slice(
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>\nendobj\n",
        );
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n2 1\n{root:010} 00000 n\r\n5 1\n{page:010} 00000 n\r\n\
                 trailer\n<< /Size 6 /Root 1 0 R /Prev {first_xref} >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        assert_eq!(page_count(&pdf), Some(3));
    }

    /// Objects 1.. where the page tree lives in a compressed object
    /// stream, with a cross-reference stream using a PNG predictor.
    fn compressed_pdf(pages: usize) -> Vec<u8> {
        let mut out = b"%PDF-1.5\n".to_vec();
        let catalog = out.len();
        out.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        // Object stream 3 holds 2 (the tree root) and 4.. (the pages).
        let members: Vec<(u32, String)> = std::iter::once((
            2,
            format!(
                "<< /Type /Pages /Kids [{}] /Count {pages} >>",
                (0..pages)
                    .map(|i| format!("{} 0 R", i + 4))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        ))
        .chain((0..pages).map(|i| (i as u32 + 4, "<< /Type /Page /Parent 2 0 R >>".into())))
        .collect();
        let mut header = String::new();
        let mut body = String::new();
        members.iter().for_each(|(number, object)| {
            header.push_str(&format!("{number} {} ", body.len()));
            body.push_str(object);
            body.push(' ');
        });
        let content = zlib(format!("{header}{body}").as_bytes());
        let stream = out.len();
        out.extend_from_slice(
            format!(
                "3 0 obj\n<< /Type /ObjStm /N {} /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
                members.len(),
                header.len(),
                content.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&content);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        let xref_number = pages as u32 + 4;
        let xref = out.len();
        // Rows of (type, field 2 in 2 bytes, field 3), PNG "up" filtered.
        let mut rows: Vec<[u8; 4]> = vec![[0, 0, 0, 255]];
        rows.push([1, (catalog >> 8) as u8, catalog as u8, 0]);
        rows.push([2, 0, 3, 0]);
        rows.push([1, (stream >> 8) as u8, stream as u8, 0]);
        (0..pages).for_each(|i| rows.push([2, 0, 3, i as u8 + 1]));
        rows.push([1, (xref >> 8) as u8, xref as u8, 0]);
        let mut previous = [0u8; 4];
        let filtered: Vec<u8> = rows
            .iter()
            .flat_map(|row| {
                let up: Vec<u8> = row
                    .iter()
                    .zip(previous)
                    .map(|(b, p)| b.wrapping_sub(p))
                    .collect();
                previous = *row;
                std::iter::once(2u8).chain(up)
            })
            .collect();
        let data = zlib(&filtered);
        out.extend_from_slice(
            format!(
                "{xref_number} 0 obj\n<< /Type /XRef /Size {} /W [1 2 1] /Root 1 0 R \
                 /Filter /FlateDecode /DecodeParms << /Columns 4 /Predictor 12 >> /Length {} >>\nstream\r\n",
                rows.len(),
                data.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&data);
        out.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        out
    }

    #[test]
    fn cross_reference_streams_and_object_streams() {
        assert_eq!(page_count(&compressed_pdf(5)), Some(5));
    }

    #[test]
    fn a_damaged_table_is_rebuilt_by_scanning() {
        // Every offset is wrong: the scan finds the objects.
        let mut pdf = classic_pdf(&flat_tree(4, 4), "/Root 1 0 R");
        let at = find(&pdf, b"xref\n", 0).unwrap();
        let table = String::from_utf8_lossy(&pdf[at..]).replace("0000000", "0000001");
        pdf.truncate(at);
        pdf.extend_from_slice(table.as_bytes());
        assert_eq!(page_count(&pdf), Some(4));
        // No usable startxref at all, and an object stream to scan into.
        let mut pdf = compressed_pdf(3);
        let at = rfind(&pdf, b"startxref").unwrap();
        pdf.truncate(at);
        assert_eq!(page_count(&pdf), Some(3));
    }

    #[test]
    fn unknown_filters_are_not_guessed() {
        // The object stream claims LZW: neither the table nor the scan
        // can read the page tree inside it.
        // (same length, so every offset stays right)
        let mut pdf = compressed_pdf(2);
        let stream = find(&pdf, b"/Type /ObjStm", 0).unwrap();
        let at = find(&pdf, b"/FlateDecode", stream).unwrap();
        pdf[at..at + 12].copy_from_slice(b"/LZWDecode  ");
        assert_eq!(page_count(&pdf), None);
    }

    #[test]
    fn png_predictors() {
        // Sub, up, average and paeth rows of three columns, one byte each.
        let rows = [[1u8, 5, 1, 1], [2, 1, 1, 1], [3, 2, 2, 2], [4, 1, 1, 1]];
        let data: Vec<u8> = rows.iter().flatten().copied().collect();
        let params: Dict = vec![
            (b"Predictor".to_vec(), Obj::Int(15)),
            (b"Columns".to_vec(), Obj::Int(3)),
        ];
        assert_eq!(
            unpredict(&data, Some(&params)).unwrap(),
            [5, 6, 7, 6, 7, 8, 5, 8, 10, 6, 9, 11]
        );
        assert_eq!(
            unpredict(&[9, 1], Some(&params)),
            None,
            "an unknown row filter"
        );
    }

    /// Damaged input never panics: every truncation and a sweep of byte
    /// changes over both kinds of file (results are not checked, only
    /// that each call returns).
    #[test]
    fn damaged_files_never_panic() {
        [
            classic_pdf(&flat_tree(3, 3), "/Root 1 0 R"),
            compressed_pdf(4),
        ]
        .iter()
        .for_each(|pdf| {
            (0..pdf.len()).for_each(|end| {
                page_count(&pdf[..end]);
            });
            (0..pdf.len()).step_by(3).for_each(|at| {
                [b'0', b' ', b'/', b'<', b'(', b'[', 0xff]
                    .iter()
                    .for_each(|&byte| {
                        let mut damaged = pdf.clone();
                        damaged[at] = byte;
                        page_count(&damaged);
                    });
            });
        });
    }

    #[test]
    fn names_decode_their_escapes() {
        assert_eq!(decode_name(b"Ty#70e"), b"Type");
        assert_eq!(decode_name(b"A#2"), b"A#2");
    }
}
