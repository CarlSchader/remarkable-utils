//! The PDF page counter against hostile files at full size, under its
//! default limits: memory measured by a counting global allocator (the
//! peak of live heap bytes during each count), time by the clock. One
//! test, so nothing else allocates meanwhile. Every file is fabricated.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use libremarkable_utils::pdf::page_count;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as is.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded as is.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded as is.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(new_size, Ordering::Relaxed) + new_size;
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The most heap one of these counts may use. Measured: 33 MiB for a
/// decoded stream at the 16 MiB cap (grown by doubling), about 67 MiB
/// for a cross-reference stream filling the million-entry cap.
const PEAK_LIMIT: usize = 128 << 20;

/// The longest one count may take. Measured: under 50 ms optimised and
/// under a second unoptimised; generous for slow machines.
fn time_limit() -> Duration {
    match cfg!(debug_assertions) {
        true => Duration::from_secs(20),
        false => Duration::from_secs(2),
    }
}

/// Count `pdf`, returning the result, the peak heap above what was live
/// before, and the time it took.
fn measure(pdf: &[u8]) -> (Option<usize>, usize, Duration) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let started = Instant::now();
    let pages = page_count(pdf);
    let took = started.elapsed();
    (pages, PEAK.load(Ordering::Relaxed) - before, took)
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// A Flate stream of `len` copies of `byte`, compressed in chunks of one
/// repeated block (fast to build even unoptimised).
fn repeated(byte: u8, len: usize) -> Vec<u8> {
    let chunk = vec![byte; 1 << 20];
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut left = len;
    while left > 0 {
        let take = left.min(chunk.len());
        encoder.write_all(&chunk[..take]).unwrap();
        left -= take;
    }
    encoder.finish().unwrap()
}

fn xref_section(records: usize, index: Option<(usize, usize)>, prev: Option<usize>) -> Vec<u8> {
    let z = repeated(1, records);
    let index = index.map_or(String::new(), |(start, count)| {
        format!(" /Index [{start} {count}]")
    });
    let prev = prev.map_or(String::new(), |prev| format!(" /Prev {prev}"));
    let mut out = format!(
        "1 0 obj\n<< /Type /XRef /W [0 1 0] /Size {records}{index}{prev} /Root 1 0 R \
         /Filter /FlateDecode /Length {} >>\nstream\n",
        z.len()
    )
    .into_bytes();
    out.extend(&z);
    out.extend(b"\nendstream\nendobj\n");
    out
}

/// A cross-reference stream of `records` type-2 entries (each kept, in
/// an object stream that does not exist): the entry cap filled.
fn full_table(records: usize) -> Vec<u8> {
    let data: Vec<u8> = (0..records)
        .flat_map(|n| [2u8, 0, (n % 200) as u8])
        .collect();
    let z = zlib(&data);
    let mut pdf = format!(
        "%PDF-1.5\n1 0 obj\n<< /Type /XRef /W [1 1 1] /Size {records} /Root 1 0 R \
         /Filter /FlateDecode /Length {} >>\nstream\n",
        z.len()
    )
    .into_bytes();
    pdf.extend(&z);
    pdf.extend(b"\nendstream\nendobj\nstartxref\n9\n%%EOF\n");
    pdf
}

fn xref_bomb(records: usize) -> Vec<u8> {
    let mut pdf = b"%PDF-1.5\n".to_vec();
    pdf.extend(xref_section(records, None, None));
    pdf.extend(b"startxref\n9\n%%EOF\n");
    pdf
}

fn chained_sections(sections: usize, records: usize) -> Vec<u8> {
    let mut pdf = b"%PDF-1.5\n".to_vec();
    let mut prev = None;
    (0..sections).for_each(|n| {
        let at = pdf.len();
        pdf.extend(xref_section(records, Some((n * records, records)), prev));
        prev = Some(at);
    });
    pdf.extend(format!("startxref\n{}\n%%EOF\n", prev.unwrap()).bytes());
    pdf
}

/// Pages 10.. in object stream 3, every member at offset 0 of its data,
/// a Page dictionary carrying an array of `pad` integers; with a table,
/// or for the scan without one.
fn object_stream_pages(kids: usize, pad: usize, table: bool) -> Vec<u8> {
    let refs: Vec<String> = (0..kids).map(|i| format!("{} 0 R", i + 10)).collect();
    let header: String = (0..kids).map(|i| format!("{} 0 ", i + 10)).collect();
    let mut data = format!("{header}<< /Type /Page /Pad [").into_bytes();
    data.extend(b"1 ".repeat(pad));
    data.extend(b"] >>");
    let z = zlib(&data);
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
    if table {
        let xref = pdf.len();
        pdf.extend(
            format!(
                "xref\n0 4\n0000000000 65535 f\r\n{catalog:010} 00000 n\r\n{root:010} 00000 n\r\n\
                 {stream:010} 00000 n\r\ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .bytes(),
        );
    }
    pdf
}

/// `N` objects `k 0 obj<tail>` and nothing else (the scan path).
fn repeated_objects(n: usize, tail: &str) -> Vec<u8> {
    (1..=n).fold(b"%PDF-1.4\n".to_vec(), |mut pdf, k| {
        pdf.extend(format!("{k} 0 obj{tail}").bytes());
        pdf
    })
}

/// A valid table over `n` pages, each followed by `stream` and no
/// `endstream`.
fn pages_without_endstream(n: usize) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
    let objects: Vec<String> = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
    ]
    .into_iter()
    .chain((0..n).map(|_| "<< /Type /Page /Parent 2 0 R >>\nstream\n".to_string()))
    .collect();
    let offsets: Vec<usize> = objects
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let at = pdf.len();
            pdf.extend(format!("{} 0 obj\n{body}\nendobj\n", i + 1).bytes());
            at
        })
        .collect();
    let xref = pdf.len();
    pdf.extend(format!("xref\n0 {}\n0000000000 65535 f\r\n", objects.len() + 1).bytes());
    offsets
        .iter()
        .for_each(|at| pdf.extend(format!("{at:010} 00000 n\r\n").bytes()));
    pdf.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .bytes(),
    );
    pdf
}

#[test]
fn hostile_files_stay_within_memory_and_time() {
    let cases: Vec<(&str, Vec<u8>, Option<usize>)> = vec![
        ("xref stream decoding to 63 MiB", xref_bomb(63 << 20), None),
        ("xref stream decoding to 16 MiB", xref_bomb(16 << 20), None),
        ("a million kept entries", full_table(1 << 20), None),
        (
            "8 chained xref streams of 4 MiB",
            chained_sections(8, 4 << 20),
            None,
        ),
        (
            "a page carrying a 60 MiB array",
            object_stream_pages(1, 30 << 20, true),
            None,
        ),
        (
            "40 pages sharing a member with a 16 MiB array",
            object_stream_pages(40, 8 << 20, false),
            None,
        ),
        (
            "80,000 streams without endstream",
            repeated_objects(80_000, "<<>>stream\n"),
            None,
        ),
        (
            "unterminated hex strings",
            repeated_objects(100_000, " [<\n"),
            None,
        ),
        (
            "unterminated literal strings",
            repeated_objects(100_000, " (\n"),
            None,
        ),
        (
            "40,000 valid pages followed by stream",
            pages_without_endstream(40_000),
            Some(40_000),
        ),
    ];
    cases.iter().for_each(|(what, pdf, expected)| {
        let (pages, peak, took) = measure(pdf);
        eprintln!(
            "{what}: {} KiB file, {pages:?}, peak heap {} KiB, {took:.2?}",
            pdf.len() / 1024,
            peak / 1024
        );
        assert_eq!(pages, *expected, "{what}");
        assert!(peak <= PEAK_LIMIT, "{what}: peak heap {peak} bytes");
        assert!(took <= time_limit(), "{what}: took {took:?}");
    });
}
