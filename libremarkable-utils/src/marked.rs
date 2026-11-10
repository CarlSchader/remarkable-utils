//! Marker-sectioned output: one remote script answers several questions
//! in one SSH round trip, each answer behind a header line
//! `<marker> <name>` (printed as `printf '\n%s %s\n' <marker> <name>`).
//! The marker is random per call ([`new_marker`]), so file contents can
//! never forge a header.
//!
//! Two readers, with different body rules that their callers rely on:
//! [`split_sections`] (every section, each body line newline-terminated,
//! for the listings and `rmu status`) and [`section`] (one section,
//! exactly the bytes before the next header, for the hub probe).

/// A fresh marker, `===RMU:<random hex>===`.
pub(crate) fn new_marker() -> String {
    format!("===RMU:{}===", uuid::Uuid::new_v4().simple())
}

/// Split marker-delimited output into `(section name, body)` pairs, in
/// order. A header is a line that starts with `<marker> `; the name is
/// the rest of it, trimmed. Every other line is appended to the current
/// body with a `\n`; lines before the first header are dropped.
pub(crate) fn split_sections(marker: &str, output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .fold(Vec::<(String, String)>::new(), |mut sections, line| {
            match line
                .strip_prefix(marker)
                .and_then(|rest| rest.strip_prefix(' '))
            {
                Some(name) => sections.push((name.trim().to_string(), String::new())),
                None => {
                    if let Some((_, body)) = sections.last_mut() {
                        body.push_str(line);
                        body.push('\n');
                    }
                }
            }
            sections
        })
}

/// The body of the first section `name` in `output`: everything after
/// its header `\n<marker> <name>\n` up to the newline that starts the
/// next header (or the end); `None` if it is missing.
pub(crate) fn section<'a>(marker: &str, output: &'a str, name: &str) -> Option<&'a str> {
    let header = format!("\n{marker} {name}\n");
    let start = output.find(&header)? + header.len();
    let body = &output[start..];
    // The newline before the next header belongs to that header.
    let end = body.find(&format!("\n{marker} ")).unwrap_or(body.len());
    Some(&body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "===RMU:0123===";

    /// As a script printing `free` into section `lock` and `b1` into
    /// `boot`, with output before and after.
    fn output() -> String {
        format!("noise\n\n{M} lock\nfree\n\n{M} boot\nb1\ntrailer\n")
    }

    #[test]
    fn markers_are_fresh_and_well_formed() {
        let (a, b) = (new_marker(), new_marker());
        assert_ne!(a, b);
        let hex = a
            .strip_prefix("===RMU:")
            .and_then(|rest| rest.strip_suffix("==="))
            .unwrap();
        assert_eq!(hex.len(), 32);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn split_sections_keeps_every_line_of_every_body() {
        assert_eq!(
            split_sections(M, &output()),
            vec![
                ("lock".to_string(), "free\n\n".to_string()),
                ("boot".to_string(), "b1\ntrailer\n".to_string()),
            ]
        );
        // A header needs the space; names are trimmed; repeats are kept.
        assert_eq!(
            split_sections(M, &format!("{M}x\n{M}  a \n1\n{M} a\n2")),
            vec![
                ("a".to_string(), "1\n".to_string()),
                ("a".to_string(), "2\n".to_string()),
            ]
        );
        assert!(split_sections(M, "no headers\n").is_empty());
    }

    #[test]
    fn section_is_exactly_the_bytes_before_the_next_header() {
        let output = output();
        assert_eq!(section(M, &output, "lock"), Some("free\n"));
        assert_eq!(section(M, &output, "boot"), Some("b1\ntrailer\n"));
        assert_eq!(section(M, &output, "fired"), None);
        // The header must start a line after a newline.
        assert_eq!(section(M, &format!("{M} lock\nfree\n"), "lock"), None);
        // The first of two sections with one name.
        assert_eq!(
            section(M, &format!("\n{M} a\n1\n{M} a\n2\n"), "a"),
            Some("1")
        );
    }
}
