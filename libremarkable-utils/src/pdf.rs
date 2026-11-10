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
//! cyclic tree) is `None`, never a guess.
//!
//! Hostile input is expected (`rmu hub add` is fed files from anywhere),
//! so every count runs within [`Limits`]: a work budget (every byte
//! searched, parsed or decoded and every object built costs a unit),
//! caps on one decoded stream, on the object streams held decoded, on
//! cross-reference entries (checked before anything is allocated for
//! them), on the objects one parse builds, and on page-tree nodes, plus
//! nesting, hop, section and depth limits. Running out of any of them is
//! `None`. Stream data is only read where it is needed (cross-reference
//! and object streams), and a hex string ends at its first non-hex
//! byte, so no object makes the parser search the rest of the file.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::rc::Rc;

/// Nesting of arrays and dictionaries the parser follows.
const MAX_NESTING: usize = 64;
/// Cross-reference sections followed through `/Prev` and `/XRefStm`.
const MAX_SECTIONS: usize = 256;
/// Hops through indirect references to reach a value.
const MAX_HOPS: usize = 32;
/// Depth of the page tree.
const MAX_DEPTH: usize = 256;
/// Digits (or white space) of an `N G obj` header looked at, per field.
const MAX_HEADER_RUN: usize = 32;
/// How far into the file `%PDF-` may start.
const HEADER_WINDOW: usize = 1024;

/// What one page count may use (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Work units: bytes searched, parsed and decoded, objects built.
    pub work: u64,
    /// Bytes one decoded stream may hold.
    pub decoded: usize,
    /// Bytes of decoded object streams held at once.
    pub cached: usize,
    /// Cross-reference entries (and object-stream members) kept, across
    /// every section and the scan.
    pub entries: usize,
    /// Objects one parse builds, nested ones and array and dictionary
    /// elements included.
    pub elements: usize,
    /// Page-tree nodes visited or queued.
    pub nodes: usize,
}

impl Limits {
    /// The limits for a file of `len` bytes: a work budget of 16 units
    /// per byte plus 64 Mi (room for a few decoded streams), 16 MiB per
    /// decoded stream (a million-object cross-reference stream takes
    /// about 7), 32 MiB of object streams, a million entries, 256 Ki
    /// objects per parse and a million page-tree nodes. Peak memory stays
    /// in the low hundreds of MiB whatever the input.
    pub fn for_input(len: usize) -> Self {
        Self {
            work: u64::try_from(len)
                .unwrap_or(u64::MAX)
                .saturating_mul(16)
                .saturating_add(64 << 20),
            decoded: 16 << 20,
            cached: 32 << 20,
            entries: 1 << 20,
            elements: 1 << 18,
            nodes: 1 << 20,
        }
    }
}

/// Whether `bytes` start like a PDF: `%PDF-` within the first KiB.
pub fn is_pdf(bytes: &[u8]) -> bool {
    header(bytes).is_some()
}

fn header(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(5)
        .take(HEADER_WINDOW)
        .position(|window| window == b"%PDF-")
}

/// The number of pages of a PDF, if it can be read reliably within
/// [`Limits::for_input`] (see the module docs); `None` otherwise.
pub fn page_count(pdf: &[u8]) -> Option<usize> {
    page_count_within(pdf, Limits::for_input(pdf.len()))
}

/// [`page_count`] within other limits.
pub fn page_count_within(pdf: &[u8], limits: Limits) -> Option<usize> {
    count(pdf, limits).0
}

/// What a count used, for tests of the limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Spent {
    work: u64,
    /// The most cross-reference entries kept at once.
    entries: usize,
    /// The most decoded object-stream bytes held at once.
    cached: usize,
}

fn count(pdf: &[u8], limits: Limits) -> (Option<usize>, Spent) {
    let budget = Budget::new(limits);
    let pages = (|| {
        let header = header(pdf)?;
        // Offsets are kept as `u32`.
        u32::try_from(pdf.len()).ok()?;
        let from_xref = startxref(&budget, pdf)
            .and_then(|offset| load_xref(&budget, pdf, offset, header))
            .and_then(|xref| Resolver::new(&budget, pdf, xref).page_count());
        if from_xref.is_some() {
            return from_xref;
        }
        // What the first attempt kept is dropped; the work spent is not.
        budget.release();
        let xref = scan(&budget, pdf)?;
        Resolver::new(&budget, pdf, xref).page_count()
    })();
    (pages, budget.spent())
}

// ---- the budget ---------------------------------------------------------------

/// The limits of one count and what it used so far.
struct Budget {
    limits: Limits,
    work: Cell<u64>,
    entries: Cell<usize>,
    cached: Cell<usize>,
    peak: Cell<Spent>,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self {
            limits,
            work: Cell::new(0),
            entries: Cell::new(0),
            cached: Cell::new(0),
            peak: Cell::new(Spent::default()),
        }
    }

    /// Spend `units` of work; `None` once the budget is gone (it stays
    /// gone).
    fn spend(&self, units: usize) -> Option<()> {
        let spent = self
            .work
            .get()
            .saturating_add(u64::try_from(units).unwrap_or(u64::MAX));
        self.work.set(spent);
        (spent <= self.limits.work).then_some(())
    }

    fn exhausted(&self) -> bool {
        self.work.get() > self.limits.work
    }

    /// Work units left, as a byte count.
    fn left(&self) -> usize {
        usize::try_from(self.limits.work.saturating_sub(self.work.get())).unwrap_or(usize::MAX)
    }

    /// Room for `count` more entries (before they are allocated).
    fn take_entries(&self, count: usize) -> Option<()> {
        let kept = self
            .entries
            .get()
            .checked_add(count)
            .filter(|&kept| kept <= self.limits.entries)?;
        self.entries.set(kept);
        self.note();
        Some(())
    }

    /// Room for `bytes` more of decoded object streams.
    fn take_cached(&self, bytes: usize) -> Option<()> {
        let held = self
            .cached
            .get()
            .checked_add(bytes)
            .filter(|&held| held <= self.limits.cached)?;
        self.cached.set(held);
        self.note();
        Some(())
    }

    /// Everything kept was dropped (the table is built again).
    fn release(&self) {
        self.entries.set(0);
        self.cached.set(0);
    }

    fn note(&self) {
        let peak = self.peak.get();
        self.peak.set(Spent {
            work: 0,
            entries: peak.entries.max(self.entries.get()),
            cached: peak.cached.max(self.cached.get()),
        });
    }

    fn spent(&self) -> Spent {
        Spent {
            work: self.work.get(),
            ..self.peak.get()
        }
    }

    /// The first `needle` in `data` from `from`, paying for the bytes
    /// searched; never searches further than the budget allows.
    fn find(&self, data: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        let rest = data.get(from..)?;
        let window = rest.len().min(self.left().saturating_add(needle.len()));
        let found = rest[..window]
            .windows(needle.len())
            .position(|candidate| candidate == needle);
        self.spend(found.map_or(window, |at| at + needle.len()))?;
        found.map(|at| at + from)
    }

    /// The last `needle` in `data`.
    fn rfind(&self, data: &[u8], needle: &[u8]) -> Option<usize> {
        self.spend(data.len())?;
        data.windows(needle.len())
            .rposition(|candidate| candidate == needle)
    }
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

/// Reads objects from `data`, paying the budget for every byte it moves
/// over and every object it builds; once the budget is gone it sees the
/// end of the data, so every parse fails.
struct Parser<'a, 'b> {
    data: &'a [u8],
    pos: usize,
    budget: &'b Budget,
    /// Objects built by this parser.
    built: usize,
}

impl<'a, 'b> Parser<'a, 'b> {
    fn at(budget: &'b Budget, data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos: pos.min(data.len()),
            budget,
            built: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        match self.budget.exhausted() {
            true => None,
            false => self.data.get(self.pos).copied(),
        }
    }

    /// Pay for `bytes` moved over (the budget makes the rest unreadable
    /// once it is gone).
    fn pay(&self, bytes: usize) {
        let _ = self.budget.spend(bytes);
    }

    fn skip_white(&mut self) {
        let start = self.pos;
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
        self.pay(self.pos - start);
    }

    /// The run of regular characters at the position.
    fn word(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self.peek().is_some_and(is_regular) {
            self.pos += 1;
        }
        self.pay(self.pos - start + 1);
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
        self.built += 1;
        if depth > MAX_NESTING || self.built > self.budget.limits.elements {
            return None;
        }
        self.pay(1);
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
            b'<' => self.hex_string().map(|()| Obj::Str),
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

    /// `<hex digits and white space>`: anything else ends it as broken.
    fn hex_string(&mut self) -> Option<()> {
        let start = self.pos;
        self.pos += 1;
        let closed = loop {
            match self.peek() {
                Some(b'>') => break true,
                Some(byte) if byte.is_ascii_hexdigit() || is_white(byte) => self.pos += 1,
                _ => break false,
            }
        };
        self.pay(self.pos - start);
        closed.then(|| self.pos += 1)
    }

    fn literal_string(&mut self) -> Option<()> {
        let start = self.pos;
        self.pos += 1;
        let mut open = 1usize;
        while open > 0 {
            let Some(byte) = self.peek() else {
                self.pay(self.pos - start);
                return None;
            };
            match byte {
                b'\\' => self.pos += 1,
                b'(' => open += 1,
                b')' => open -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        self.pay(self.pos - start);
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

    /// `N G obj <value>` at the position, its number and value. A stream
    /// after it is not read: [`Self::stream`] does that, only where the
    /// data is needed.
    fn indirect(&mut self) -> Option<(u32, Obj)> {
        let number = u32::try_from(self.unsigned()?).ok()?;
        self.unsigned()?;
        if !self.keyword(b"obj") {
            return None;
        }
        Some((number, self.object(0)?))
    }

    /// Right after an object's dictionary: `stream ... endstream`, the
    /// raw data, by a direct `/Length` that ends at `endstream`, else up
    /// to the next `endstream`.
    fn stream(&mut self, dict: &Dict) -> Option<&'a [u8]> {
        if !self.keyword(b"stream") {
            return None;
        }
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
                let mut after = Parser::at(self.budget, self.data, end);
                end <= self.data.len() && after.keyword(b"endstream")
            });
        let end = match by_length {
            Some(end) => end,
            None => {
                let mut end = self.budget.find(self.data, b"endstream", start)?;
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
fn decode_stream(budget: &Budget, dict: &Dict, raw: &[u8]) -> Option<Vec<u8>> {
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
        [] if raw.len() <= budget.limits.decoded => {
            budget.spend(raw.len())?;
            Some(raw.to_vec())
        }
        [b"FlateDecode" | b"Fl"] => {
            let inflated = inflate(budget, raw)?;
            budget.spend(inflated.len())?;
            unpredict(&inflated, params)
        }
        _ => None,
    }
}

/// Inflate at most [`Limits::decoded`] bytes (and no more than the work
/// budget left): a stream that would decode to more is `None`.
fn inflate(budget: &Budget, raw: &[u8]) -> Option<Vec<u8>> {
    budget.spend(raw.len())?;
    let cap = budget.limits.decoded.min(budget.left());
    let mut out = Vec::new();
    let mut reader = flate2::read::ZlibDecoder::new(raw).take(cap as u64 + 1);
    // A truncated or checksum-damaged stream still yields what it
    // decoded; the page-tree walk judges whether it made sense.
    let read = reader.read_to_end(&mut out);
    budget.spend(out.len())?;
    if out.len() > cap || (read.is_err() && out.is_empty()) {
        return None;
    }
    out.shrink_to_fit();
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
    let bits =
        usize::try_from(param(b"Colors", 1).checked_mul(param(b"BitsPerComponent", 8))?).ok()?;
    let columns = usize::try_from(param(b"Columns", 1)).ok()?;
    let bpp = (bits / 8).max(1);
    let row = bits.checked_mul(columns)?.div_ceil(8);
    // A row longer than the data cannot be (and must not allocate).
    if row == 0 || row > data.len() {
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

/// Where an object is. Offsets are `u32` (files over 4 GiB are not
/// counted), which keeps a million entries in about 32 MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// At a byte offset.
    Offset(u32),
    /// Inside object stream `stream`, at `index`.
    InStream { stream: u32, index: u32 },
}

#[derive(Debug, Default)]
struct Xref {
    entries: HashMap<u32, Entry>,
    /// The newest trailer's `/Root`.
    root: Option<Obj>,
}

/// The offset `startxref` names.
fn startxref(budget: &Budget, pdf: &[u8]) -> Option<usize> {
    let at = budget.rfind(pdf, b"startxref")?;
    let mut parser = Parser::at(budget, pdf, at + b"startxref".len());
    usize::try_from(parser.unsigned()?).ok()
}

/// Every cross-reference section from `offset` on, newest first (an
/// entry a newer section has wins). Offsets are tried as written, then
/// relative to the header, for files with bytes before `%PDF-`.
fn load_xref(budget: &Budget, pdf: &[u8], offset: usize, header: usize) -> Option<Xref> {
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
        let section = section(budget, pdf, offset).or_else(|| {
            (header > 0)
                .then(|| section(budget, pdf, offset.checked_add(header)?))
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
fn section(budget: &Budget, pdf: &[u8], offset: usize) -> Option<Section> {
    let mut parser = Parser::at(budget, pdf, offset);
    if parser.keyword(b"xref") {
        return classic(&mut parser);
    }
    let mut parser = Parser::at(budget, pdf, offset);
    let (_, Obj::Dict(dict)) = parser.indirect()? else {
        return None;
    };
    if get(&dict, b"Type").and_then(Obj::name) != Some(b"XRef") {
        return None;
    }
    let raw = parser.stream(&dict)?;
    let data = decode_stream(budget, &dict, raw)?;
    Some(Section {
        entries: stream_entries(budget, &dict, &data, pdf.len())?,
        trailer: dict,
    })
}

/// A classic table after `xref`: subsections of `start count` and
/// `count` entries `offset generation n|f`, then `trailer << ... >>`.
/// Free entries, and offsets past the end, are left out (a missing entry
/// is free). Each subsection's count must fit the entry cap first.
fn classic(parser: &mut Parser) -> Option<Section> {
    let len = parser.data.len();
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
        parser.budget.take_entries(usize::try_from(count).ok()?)?;
        for number in start..start.checked_add(count)? {
            let offset = usize::try_from(parser.unsigned()?).ok()?;
            parser.unsigned()?;
            parser.skip_white();
            match parser.word() {
                b"n" if offset > 0 && offset < len => {
                    entries.push((number, Entry::Offset(u32::try_from(offset).ok()?)))
                }
                b"n" | b"f" => {}
                _ => return None,
            }
        }
    }
}

/// The entries of a decoded cross-reference stream (`/W`, `/Index`):
/// the counts must fit the data and the entry cap before anything is
/// allocated; offsets past the end of the file are left out.
fn stream_entries(
    budget: &Budget,
    dict: &Dict,
    data: &[u8],
    len: usize,
) -> Option<Vec<(u32, Entry)>> {
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
    let total = ranges.iter().try_fold(0usize, |total, (_, count)| {
        total.checked_add(usize::try_from(*count).ok()?)
    })?;
    if total.checked_mul(record)? > data.len() {
        return None;
    }
    budget.take_entries(total)?;
    budget.spend(total)?;
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
                1 if second > 0 && second < len as u64 => {
                    entries.push((number, Entry::Offset(u32::try_from(second).ok()?)))
                }
                2 => entries.push((
                    number,
                    Entry::InStream {
                        stream: u32::try_from(second).ok()?,
                        index: u32::try_from(third).ok()?,
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
/// `/Type /Catalog`. Only object-stream data is read.
fn scan(budget: &Budget, pdf: &[u8]) -> Option<Xref> {
    let mut entries = HashMap::new();
    let mut at = 0;
    while let Some(found) = budget.find(pdf, b"obj", at) {
        at = found + 3;
        if pdf.get(at).is_some_and(|&b| is_regular(b)) {
            continue;
        }
        if let Some((number, start)) = object_header_before(pdf, found) {
            budget.take_entries(1)?;
            entries.insert(number, Entry::Offset(u32::try_from(start).ok()?));
        }
    }
    if budget.exhausted() {
        return None;
    }
    let mut direct: Vec<(u32, u32)> = entries
        .iter()
        .filter_map(|(&number, entry)| match entry {
            Entry::Offset(offset) => Some((number, *offset)),
            Entry::InStream { .. } => None,
        })
        .collect();
    direct.sort_by_key(|&(_, offset)| offset);
    let mut root = None;
    let mut catalog = None;
    for (number, offset) in direct {
        let mut parser = Parser::at(budget, pdf, offset as usize);
        let Some((_, Obj::Dict(dict))) = parser.indirect() else {
            continue;
        };
        match get(&dict, b"Type").and_then(Obj::name) {
            Some(b"Catalog") => catalog = Some(Obj::Ref(number)),
            Some(b"XRef") => root = get(&dict, b"Root").cloned().or(root),
            Some(b"ObjStm") => {
                let members = parser
                    .stream(&dict)
                    .and_then(|raw| decode_stream(budget, &dict, raw))
                    .and_then(|data| object_stream_header(budget, &dict, &data));
                members
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .for_each(|(index, (member, _))| {
                        entries.entry(member).or_insert(Entry::InStream {
                            stream: number,
                            index: index as u32,
                        });
                    });
            }
            _ => {}
        }
    }
    let mut trailer_at = 0;
    while let Some(found) = budget.find(pdf, b"trailer", trailer_at) {
        trailer_at = found + 7;
        if let Some(Obj::Dict(dict)) = Parser::at(budget, pdf, trailer_at).object(0)
            && let Some(found) = get(&dict, b"Root")
        {
            root = Some(found.clone());
        }
    }
    if budget.exhausted() {
        return None;
    }
    Some(Xref {
        entries,
        root: Some(root.or(catalog)?),
    })
}

/// The object number and start of an `N G obj` header ending right
/// before `obj` at `at` (each run of digits or white space looked at is
/// at most [`MAX_HEADER_RUN`] bytes).
fn object_header_before(pdf: &[u8], at: usize) -> Option<(u32, usize)> {
    let run_before = |end: usize, keep: fn(u8) -> bool| -> Option<usize> {
        let floor = end.saturating_sub(MAX_HEADER_RUN);
        let start = pdf[floor..end]
            .iter()
            .rposition(|&b| !keep(b))
            .map_or(floor, |p| floor + p + 1);
        (start < end).then_some(start)
    };
    let digit = |b: u8| b.is_ascii_digit();
    let generation = run_before(run_before(at, is_white)?, digit)?;
    let number_end = run_before(generation, is_white)?;
    let number = run_before(number_end, digit)?;
    if number > 0 && is_regular(pdf[number - 1]) {
        return None;
    }
    let text = std::str::from_utf8(&pdf[number..number_end]).ok()?;
    Some((text.parse().ok()?, number))
}

/// An object stream's header: `/N` pairs of object number and offset
/// (relative to `/First`). `/N` must fit the header (each pair takes at
/// least four bytes) and the entry cap before anything is allocated.
fn object_stream_header(budget: &Budget, dict: &Dict, data: &[u8]) -> Option<Vec<(u32, usize)>> {
    let count = usize::try_from(get(dict, b"N").and_then(Obj::int)?).ok()?;
    let first = usize::try_from(get(dict, b"First").and_then(Obj::int)?).ok()?;
    let header = data.get(..first)?;
    if count > header.len().div_ceil(4) {
        return None;
    }
    budget.take_entries(count)?;
    let mut parser = Parser::at(budget, header, 0);
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

struct Resolver<'a, 'b> {
    budget: &'b Budget,
    pdf: &'a [u8],
    xref: Xref,
    streams: RefCell<HashMap<u32, Option<Rc<ObjectStream>>>>,
}

impl<'a, 'b> Resolver<'a, 'b> {
    fn new(budget: &'b Budget, pdf: &'a [u8], xref: Xref) -> Self {
        Self {
            budget,
            pdf,
            xref,
            streams: RefCell::new(HashMap::new()),
        }
    }

    /// The value of object `number`.
    fn object(&self, number: u32) -> Option<Obj> {
        match *self.xref.entries.get(&number)? {
            Entry::Offset(offset) => {
                let (found, value) =
                    Parser::at(self.budget, self.pdf, offset as usize).indirect()?;
                (found == number).then_some(value)
            }
            Entry::InStream { stream, index } => {
                let stream = self.object_stream(stream)?;
                let &(member, offset) = stream.members.get(index as usize)?;
                if member != number {
                    return None;
                }
                Parser::at(self.budget, &stream.data, offset).object(0)
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
            let mut parser = Parser::at(self.budget, self.pdf, offset as usize);
            let (_, Obj::Dict(dict)) = parser.indirect()? else {
                return None;
            };
            let raw = parser.stream(&dict)?;
            let data = decode_stream(self.budget, &dict, raw)?;
            self.budget.take_cached(data.len())?;
            let members = object_stream_header(self.budget, &dict, &data)?;
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
    /// twice, a node that is neither, or a limit reached (nodes visited
    /// and queued, depth, work).
    fn walk(&self, root: &Obj) -> Option<usize> {
        let mut stack = vec![(root.clone(), 0usize)];
        let mut seen = HashSet::new();
        let mut queued = 1usize;
        let mut leaves = 0usize;
        while let Some((node, depth)) = stack.pop() {
            self.budget.spend(1)?;
            if depth > MAX_DEPTH {
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
                    Obj::Array(kids) => {
                        queued = queued
                            .checked_add(kids.len())
                            .filter(|&queued| queued <= self.budget.limits.nodes)?;
                        stack.extend(kids.into_iter().map(|kid| (kid, depth + 1)));
                    }
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

    fn locate(data: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        data[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|at| at + from)
    }

    fn last(data: &[u8], needle: &[u8]) -> Option<usize> {
        data.windows(needle.len())
            .rposition(|window| window == needle)
    }

    fn last_startxref(pdf: &[u8]) -> usize {
        let at = last(pdf, b"startxref").unwrap() + b"startxref".len();
        let digits: String = pdf[at..]
            .iter()
            .map(|&b| b as char)
            .skip_while(|c| c.is_whitespace())
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().unwrap()
    }

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
        let first_xref = last_startxref(&pdf);
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
        let at = locate(&pdf, b"xref\n", 0).unwrap();
        let table = String::from_utf8_lossy(&pdf[at..]).replace("0000000", "0000001");
        pdf.truncate(at);
        pdf.extend_from_slice(table.as_bytes());
        assert_eq!(page_count(&pdf), Some(4));
        // No usable startxref at all, and an object stream to scan into.
        let mut pdf = compressed_pdf(3);
        let at = last(&pdf, b"startxref").unwrap();
        pdf.truncate(at);
        assert_eq!(page_count(&pdf), Some(3));
    }

    #[test]
    fn unknown_filters_are_not_guessed() {
        // The object stream claims LZW: neither the table nor the scan
        // can read the page tree inside it.
        // (same length, so every offset stays right)
        let mut pdf = compressed_pdf(2);
        let stream = locate(&pdf, b"/Type /ObjStm", 0).unwrap();
        let at = locate(&pdf, b"/FlateDecode", stream).unwrap();
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
            unpredict(&[9, 1, 1, 1], Some(&params)),
            None,
            "an unknown row filter"
        );
        // Hostile parameters: no overflow, no huge allocation.
        let hostile = |colors: i64, columns: i64| -> Dict {
            vec![
                (b"Predictor".to_vec(), Obj::Int(12)),
                (b"Colors".to_vec(), Obj::Int(colors)),
                (b"Columns".to_vec(), Obj::Int(columns)),
            ]
        };
        assert_eq!(unpredict(&data, Some(&hostile(i64::MAX, 1))), None);
        assert_eq!(unpredict(&data, Some(&hostile(1, 1 << 40))), None);
        assert_eq!(unpredict(&data, Some(&hostile(1, -1))), None);
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

    // ---- limits ---------------------------------------------------------------

    /// Small limits, so hostile inputs run out of them without needing
    /// gigabytes or minutes.
    fn small() -> Limits {
        Limits {
            work: 4 << 20,
            decoded: 1 << 20,
            cached: 2 << 20,
            entries: 1 << 14,
            elements: 1 << 12,
            nodes: 1 << 14,
        }
    }

    /// What a count may overshoot its work budget by: the last step
    /// that ran it out (one pass over the file or one decoded stream).
    fn slack(pdf: &[u8], limits: &Limits) -> u64 {
        2 * (pdf.len() + limits.decoded) as u64 + 64
    }

    /// Count within `limits` and check that the work stayed within the
    /// budget (plus the last step's slack) and every cap held.
    fn bounded(pdf: &[u8], limits: Limits) -> (Option<usize>, Spent) {
        let (pages, spent) = count(pdf, limits);
        assert!(
            spent.work <= limits.work + slack(pdf, &limits),
            "work {} over {}",
            spent.work,
            limits.work
        );
        assert!(spent.entries <= limits.entries, "{spent:?}");
        assert!(spent.cached <= limits.cached, "{spent:?}");
        (pages, spent)
    }

    /// A cross-reference stream of `records` one-byte type-1 records,
    /// Flate-compressed (`/W [0 1 0]`): a few KiB that decode to
    /// `records` bytes and as many entries.
    fn xref_bomb(records: usize, index: Option<(usize, usize)>, prev: Option<usize>) -> Vec<u8> {
        let z = zlib(&vec![1u8; records]);
        let index = index.map_or(String::new(), |(start, count)| {
            format!(" /Index [{start} {count}]")
        });
        let prev = prev.map_or(String::new(), |prev| format!(" /Prev {prev}"));
        let mut pdf = format!(
            "1 0 obj\n<< /Type /XRef /W [0 1 0] /Size {records}{index}{prev} /Root 1 0 R \
             /Filter /FlateDecode /Length {} >>\nstream\n",
            z.len()
        )
        .into_bytes();
        pdf.extend(&z);
        pdf.extend(b"\nendstream\nendobj\n");
        pdf
    }

    fn with_startxref(mut body: Vec<u8>, at: usize) -> Vec<u8> {
        body.extend(format!("startxref\n{at}\n%%EOF\n").bytes());
        body
    }

    #[test]
    fn a_cross_reference_stream_bomb_is_refused_before_it_allocates() {
        let limits = small();
        // Decodes to more than one stream may hold: refused at the cap.
        let pdf = with_startxref(
            [b"%PDF-1.5\n".as_slice(), &xref_bomb(3 << 20, None, None)].concat(),
            9,
        );
        let (pages, spent) = bounded(&pdf, limits);
        assert_eq!(pages, None);
        // No entry from the stream; the scan that follows finds object 1.
        assert_eq!(spent.entries, 1);
        // Fits the decoded cap but not the entry cap: refused by its
        // counts, before a single entry is built.
        let pdf = with_startxref(
            [b"%PDF-1.5\n".as_slice(), &xref_bomb(1 << 19, None, None)].concat(),
            9,
        );
        let (pages, spent) = bounded(&pdf, limits);
        assert_eq!(pages, None);
        assert_eq!(spent.entries, 1);
        // /Size and /Index larger than the data: refused by their counts.
        let pdf = String::from_utf8_lossy(&pdf)
            .replace("/Size 524288", "/Size 4000000000")
            .into_bytes();
        assert_eq!(bounded(&pdf, limits).0, None);
    }

    #[test]
    fn chained_sections_share_one_entry_cap() {
        let limits = small();
        // Eight sections of 4 Ki entries each, chained by /Prev, with
        // distinct /Index ranges: 32 Ki entries in all, over the cap.
        let mut pdf = b"%PDF-1.5\n".to_vec();
        let mut prev = None;
        (0..8).for_each(|n| {
            let at = pdf.len();
            pdf.extend(xref_bomb(4096, Some((n * 4096, 4096)), prev));
            prev = Some(at);
        });
        let pdf = with_startxref(pdf, prev.unwrap());
        let (pages, spent) = bounded(&pdf, limits);
        assert_eq!(pages, None);
        assert!(spent.entries <= limits.entries);
    }

    /// Catalog (1) and page tree root (2) over `kids` page objects whose
    /// numbers start at 10, all members of object stream 3, each member
    /// at offset 0 of its data, which holds `member`.
    fn object_stream_pdf(kids: usize, member: &str, scan: bool) -> Vec<u8> {
        let refs: Vec<String> = (0..kids).map(|i| format!("{} 0 R", i + 10)).collect();
        let header: String = (0..kids).map(|i| format!("{} 0 ", i + 10)).collect();
        let z = zlib(format!("{header}{member}").as_bytes());
        let mut pdf = b"%PDF-1.5\n".to_vec();
        let catalog = pdf.len();
        pdf.extend(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let root = pdf.len();
        pdf.extend(
            format!(
                "2 0 obj\n<< /Type /Pages /Kids [{}] /Count {kids} >>\nendobj\n",
                refs.join(" ")
            )
            .bytes(),
        );
        let stream = pdf.len();
        pdf.extend(
            format!(
                "3 0 obj\n<< /Type /ObjStm /N {kids} /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
                header.len(),
                z.len()
            )
            .bytes(),
        );
        pdf.extend(&z);
        pdf.extend(b"\nendstream\nendobj\n");
        if scan {
            return pdf;
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 4\n0000000000 65535 f\r\n{catalog:010} 00000 n\r\n{root:010} 00000 n\r\n{stream:010} 00000 n\r\n").bytes());
        pdf.extend(
            format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
        );
        pdf
    }

    #[test]
    fn one_huge_object_runs_out_of_its_element_cap() {
        let limits = small();
        let huge = format!(
            "<< /Type /Page /Pad [{}] >>",
            "1 ".repeat(limits.elements + 1)
        );
        // Through the scan, several pages sharing that one member.
        assert_eq!(bounded(&object_stream_pdf(10, &huge, true), limits).0, None);
        // A page with an array just under the cap still counts.
        let fits = format!(
            "<< /Type /Page /Pad [{}] >>",
            "1 ".repeat(limits.elements - 8)
        );
        assert_eq!(
            bounded(&object_stream_pdf(1, &fits, true), limits).0,
            Some(1)
        );
        // Pages sharing one small member: each parse pays.
        let (pages, spent) = bounded(
            &object_stream_pdf(500, "<< /Type /Page >>", false),
            Limits::for_input(0),
        );
        assert_eq!(pages, Some(500));
        assert!(spent.work < 1 << 20, "{spent:?}");
    }

    /// Work a count did per byte of input.
    fn work_per_byte(pdf: &[u8]) -> f64 {
        let (_, spent) = bounded(pdf, Limits::for_input(pdf.len()));
        spent.work as f64 / pdf.len() as f64
    }

    #[test]
    fn streams_without_endstream_cost_linear_work() {
        // The scan path: no startxref, `k 0 obj<<>>stream` and no
        // `endstream`, ever.
        let scan = |n: usize| -> Vec<u8> {
            (1..=n).fold(b"%PDF-1.4\n".to_vec(), |mut pdf, k| {
                pdf.extend(format!("{k} 0 obj<<>>stream\n").bytes());
                pdf
            })
        };
        let (small_work, big_work) = (work_per_byte(&scan(2_000)), work_per_byte(&scan(20_000)));
        assert!(
            big_work < 12.0 && big_work < small_work * 1.5,
            "{small_work} {big_work}"
        );
        // The table path: a valid table over pages each followed by
        // `stream` and no `endstream`; their streams are never read.
        let mut objects = flat_tree(5_000, 5_000);
        objects
            .iter_mut()
            .skip(2)
            .for_each(|page| page.push_str("\nstream\n"));
        let pdf = classic_pdf(&objects, "/Root 1 0 R");
        assert_eq!(page_count(&pdf), Some(5_000));
        assert!(work_per_byte(&pdf) < 8.0);
    }

    #[test]
    fn unterminated_strings_cost_bounded_work() {
        // Hex strings end at their first non-hex byte: linear.
        let hex = (1..=20_000).fold(b"%PDF-1.4\n".to_vec(), |mut pdf, k| {
            pdf.extend(format!("{k} 0 obj [<\n").bytes());
            pdf
        });
        assert!(work_per_byte(&hex) < 12.0);
        // Literal strings run on to the end of the file, once per
        // object: the budget stops it.
        let literal = (1..=20_000).fold(b"%PDF-1.4\n".to_vec(), |mut pdf, k| {
            pdf.extend(format!("{k} 0 obj (\n").bytes());
            pdf
        });
        let limits = Limits {
            work: 16 * literal.len() as u64,
            ..Limits::for_input(literal.len())
        };
        let (pages, _) = bounded(&literal, limits);
        assert_eq!(pages, None);
    }

    #[test]
    fn a_budget_run_out_is_none_never_a_wrong_count() {
        let pdf = compressed_pdf(5);
        let (pages, spent) = count(&pdf, Limits::for_input(pdf.len()));
        assert_eq!(pages, Some(5));
        // Every budget below what it took fails cleanly; none miscounts.
        (0..spent.work).step_by(7).for_each(|work| {
            let limits = Limits {
                work,
                ..Limits::for_input(pdf.len())
            };
            assert!(matches!(count(&pdf, limits).0, None | Some(5)), "{work}");
        });
        let limits = Limits {
            work: spent.work / 2,
            ..Limits::for_input(pdf.len())
        };
        assert_eq!(count(&pdf, limits).0, None);
    }

    #[test]
    fn page_trees_are_capped() {
        let limits = Limits {
            nodes: 100,
            ..Limits::for_input(0)
        };
        let pdf = classic_pdf(&flat_tree(150, 150), "/Root 1 0 R");
        assert_eq!(count(&pdf, limits).0, None);
        assert_eq!(page_count(&pdf), Some(150));
    }

    #[test]
    fn only_pdfs_look_like_pdfs() {
        assert!(is_pdf(b"%PDF-1.7\n"));
        assert!(is_pdf(&[b"   ".as_slice(), b"%PDF-1.4"].concat()));
        assert!(!is_pdf(b"PK\x03\x04 an epub"));
        assert!(!is_pdf(&[vec![b' '; 2000], b"%PDF-1.4".to_vec()].concat()));
    }

    /// A small deterministic generator (xorshift).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    const TOKENS: &[&[u8]] = &[
        b"obj",
        b"endobj",
        b"stream\n",
        b"endstream",
        b"xref",
        b"trailer",
        b"startxref",
        b"<<",
        b">>",
        b"[",
        b"]",
        b"(",
        b")",
        b"<",
        b"R",
        b"/Type",
        b"/Pages",
        b"/Page",
        b"/Kids",
        b"/Count",
        b"/Root",
        b"/Prev",
        b"/XRefStm",
        b"/ObjStm",
        b"/XRef",
        b"/W",
        b"/Index",
        b"/Size",
        b"/N",
        b"/First",
        b"/Filter",
        b"/FlateDecode",
        b"/Length",
        b"/DecodeParms",
        b"/Predictor",
        b"4294967295",
        b"-1",
        b"0",
        b"1",
        b"99999999999",
    ];

    /// One random mutation: flips, inserts, deletes, truncations and
    /// splices of PDF tokens.
    fn mutate(rng: &mut Rng, mut pdf: Vec<u8>) -> Vec<u8> {
        (0..1 + rng.below(4)).for_each(|_| {
            let at = rng.below(pdf.len() + 1);
            match rng.below(6) {
                0 if at < pdf.len() => pdf[at] = rng.next() as u8,
                1 => pdf.insert(at, rng.next() as u8),
                2 if at < pdf.len() => {
                    let end = (at + rng.below(32)).min(pdf.len());
                    pdf.drain(at..end);
                }
                3 => pdf.truncate(at),
                _ => {
                    let token = TOKENS[rng.below(TOKENS.len())];
                    pdf.splice(at..at, token.iter().copied());
                }
            }
        });
        pdf
    }

    fn fuzz(cases: usize, seed: u64) {
        let seeds = [
            classic_pdf(&flat_tree(3, 3), "/Root 1 0 R"),
            compressed_pdf(4),
            object_stream_pdf(3, "<< /Type /Page >>", false),
            with_startxref(
                [b"%PDF-1.5\n".as_slice(), &xref_bomb(64, None, None)].concat(),
                9,
            ),
        ];
        let mut rng = Rng(seed | 1);
        (0..cases).for_each(|case| {
            let pdf = mutate(&mut rng, seeds[case % seeds.len()].clone());
            bounded(&pdf, small());
            bounded(&pdf, Limits::for_input(pdf.len()));
        });
    }

    /// A quick fuzz pass on every test run; `fuzz_long` runs more.
    #[test]
    fn mutated_files_stay_within_their_limits() {
        fuzz(3_000, 0x5eed);
    }

    /// `RMU_PDF_FUZZ_CASES=1000000 cargo test --release -p
    /// libremarkable-utils --lib -- --ignored fuzz_long`.
    #[test]
    #[ignore = "long fuzz run; set RMU_PDF_FUZZ_CASES"]
    fn fuzz_long() {
        let cases = std::env::var("RMU_PDF_FUZZ_CASES")
            .ok()
            .and_then(|cases| cases.parse().ok())
            .unwrap_or(100_000);
        let seed = std::env::var("RMU_PDF_FUZZ_SEED")
            .ok()
            .and_then(|seed| seed.parse().ok())
            .unwrap_or(1);
        fuzz(cases, seed);
    }
}
