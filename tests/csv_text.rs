//! The text columns of a CSV report -- Server Name, Address, Share and IP --
//! are transformed, not merely escaped: characters are removed and a leading
//! apostrophe can be added, so a parser does not get back what the server
//! sent. These tests pin that, and the order the two steps run in.

use librespeed_cli::report::{csv_rows, CSVReport};

/// What a CSV parser reads back from a text column holding `value`. Reading
/// it back takes the CSV quoting out of the picture, so what is compared is
/// the stored value. All four text columns must agree on it.
fn stored(value: &str) -> String {
    let rep = CSVReport {
        name: value.into(),
        address: value.into(),
        share: value.into(),
        ip: value.into(),
        ..Default::default()
    };
    let out = csv_rows(&[rep], b',').unwrap();
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(out.as_bytes());
    let record = reader.records().next().unwrap().unwrap();
    let columns = [1, 2, 7, 8].map(|i| record.get(i).unwrap().to_string());
    assert!(columns.iter().all(|c| *c == columns[0]), "{columns:?}");
    columns[0].clone()
}

#[test]
fn sanitising_runs_before_the_formula_check() {
    // TAB is removed and is a formula trigger as well, so this one comes out
    // the same whichever step runs first.
    assert_eq!(stored("\t=cmd"), "'=cmd");

    // These tell the two orders apart. What leads is removed without being a
    // trigger, so a check made on the raw value would let it through, and
    // the formula that sanitising uncovers would be written unguarded.
    assert_eq!(stored("\u{feff}=cmd"), "'=cmd");
    assert_eq!(stored("\x1b@SUM(1)"), "'@SUM(1)");
    assert_eq!(stored("\u{200b}\u{202e}+1"), "'+1");
    assert_eq!(stored("\n-1"), "'-1");
}

#[test]
fn a_text_column_is_transformed_not_merely_escaped() {
    // Escaping round-trips: the delimiter and the quotes come back.
    assert_eq!(stored("Prague, \"CESNET\""), "Prague, \"CESNET\"");

    // Sanitising and the formula guard do not. The reader gets a value the
    // server never sent: shorter in one case, longer in the other.
    assert_eq!(stored("Pra\u{ad}gue\r\n"), "Prague");
    assert_eq!(stored("-1"), "'-1");

    // An apostrophe that was already there is not doubled or removed.
    assert_eq!(stored("'=cmd"), "'=cmd");
}
