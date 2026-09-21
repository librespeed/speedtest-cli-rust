#![no_main]
//! sanitize() is the barrier between server-supplied text and the terminal,
//! the CSV file and the JSON report.
//!
//! What is checked here holds whatever the implementation's table says: the
//! categories come from the Unicode data in icu_properties, not from a copy of
//! that table. The unit tests in src/output.rs walk every scalar value against
//! the same data; this target covers what a walk over single characters
//! cannot, which is how the filter behaves on whole strings.
use icu_properties::props::GeneralCategory as Gc;
use icu_properties::CodePointMapData;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let categories = CodePointMapData::<Gc>::new();
    let clean = librespeed_cli::output::sanitize(data);

    // Nothing of category Cc, Cf, Zl or Zp survives: those are what let a
    // server drive the terminal, reorder what a user reads, or forge a --list
    // entry.
    for c in clean.chars() {
        let gc = categories.get(c);
        assert!(
            !matches!(
                gc,
                Gc::Control | Gc::Format | Gc::LineSeparator | Gc::ParagraphSeparator
            ),
            "U+{:04X} ({gc:?}) survived sanitize",
            c as u32
        );
    }

    // The output is the input with characters taken out: nothing is added,
    // replaced or reordered. And what was taken out is one of those categories
    // or unassigned, so no character that renders is ever lost: over-filtering
    // would misreport a name, and a server whose name differs only in dropped
    // characters would read as another one. Which unassigned code points go is
    // pinned by the unit tests.
    let mut kept = clean.chars().peekable();
    for c in data.chars() {
        if kept.peek() == Some(&c) {
            kept.next();
            continue;
        }
        let gc = categories.get(c);
        assert!(
            matches!(
                gc,
                Gc::Control
                    | Gc::Format
                    | Gc::LineSeparator
                    | Gc::ParagraphSeparator
                    | Gc::Unassigned
            ),
            "U+{:04X} ({gc:?}) was dropped by sanitize",
            c as u32
        );
    }
    assert_eq!(kept.next(), None, "sanitize output is not a subsequence");

    // Filtering is idempotent, so a value that has been through it once (a
    // report field, say) cannot change again on its way out.
    assert_eq!(librespeed_cli::output::sanitize(&clean), clean);
});
