//! Best-effort excerpts of local execution diagnostics, for text output only.

use seedbox::{vmm::VMM_FAILURE_MESSAGE, Error};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const TAIL_BYTES: u64 = 8 * 1024;
const TAIL_LINES: usize = 5;

pub(super) fn ram_pages(machine: &str, pages: &seedbox::diff::PageHashDiff) {
    eprintln!("{}", ram_page_line(machine, pages));
}

fn ram_page_line(machine: &str, pages: &seedbox::diff::PageHashDiff) -> String {
    // Escape arbitrary retained names so evidence cannot introduce status lines.
    let machine: String = machine.chars().flat_map(char::escape_default).collect();
    format!(
        "ram pages: {machine}: differing pfns: {}; first pfns: {:?}",
        pages.differing_pfns, pages.first_pfns
    )
}

pub(super) fn replay_pages(comparison: &seedbox::orchestration::ReplayComparison) {
    if let Some(report) = &comparison.ram_pages {
        for (machine, pages) in &report.machines {
            ram_pages(machine, pages);
        }
        for limitation in &report.limitations {
            let safe: String = limitation.chars().flat_map(char::escape_default).collect();
            eprintln!("ram pages: {safe}");
        }
    }
}

pub(super) fn vmm_failure(directory: &Path, error: &str) {
    if error != Error::Execution(VMM_FAILURE_MESSAGE.into()).to_string() {
        return;
    }
    // Missing/unreadable diagnostics must not replace the execution outcome.
    let tail = File::open(directory.join("output/vmm.stderr")).and_then(tail);
    if let Ok(lines) = tail {
        for line in lines {
            // Prefix every line, including arbitrary VMM text, so it cannot
            // masquerade as an OUTCOME: run-directory line in script output.
            eprintln!("vmm stderr: {line}");
        }
    }
}

fn tail(mut file: impl Read + Seek) -> io::Result<Vec<String>> {
    let length = file.seek(SeekFrom::End(0))?;
    let start = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = text
        .lines()
        .enumerate()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter_map(|(index, line)| {
            // Strip terminal controls; invalid UTF-8 is rendered lossily.
            let line: String = line.chars().filter(|c| !c.is_control()).collect();
            let line = line.trim();
            if line.is_empty() {
                None
            } else if start > 0 && index == 0 {
                Some(format!("... {line}"))
            } else {
                Some(line.to_owned())
            }
        })
        .take(TAIL_LINES)
        .collect();
    lines.reverse();
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn page_diagnostics_cannot_inject_status_lines() {
        let pages = seedbox::diff::PageHashDiff {
            differing_pfns: 2,
            first_pfns: vec![3, 9],
        };
        assert_eq!(
            ram_page_line("server\nPASS: fake", &pages),
            "ram pages: server\\nPASS: fake: differing pfns: 2; first pfns: [3, 9]"
        );
    }

    #[test]
    fn reads_only_the_bounded_tail_even_with_long_lines() {
        struct Counted {
            cursor: Cursor<Vec<u8>>,
            read: usize,
        }
        impl Read for Counted {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert!(self.cursor.position() >= 1024 * 1024 - TAIL_BYTES);
                let count = self.cursor.read(buffer)?;
                self.read += count;
                Ok(count)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                self.cursor.seek(position)
            }
        }
        let mut file = Counted {
            cursor: Cursor::new(vec![b'x'; 1024 * 1024]),
            read: 0,
        };
        let lines = tail(&mut file).unwrap();
        assert_eq!(file.read, TAIL_BYTES as usize);
        assert_eq!(
            lines,
            vec![format!("... {}", "x".repeat(TAIL_BYTES as usize))]
        );
    }

    #[test]
    fn handles_empty_blank_and_non_utf8_diagnostics() {
        for bytes in [b"".as_slice(), b"\n\r\n \t \n"] {
            assert!(tail(Cursor::new(bytes)).unwrap().is_empty());
        }
        assert_eq!(
            tail(Cursor::new(b"old\n\xfffailure\x1b\0\t\n\n")).unwrap(),
            vec!["old", "\u{fffd}failure"]
        );
    }
}
