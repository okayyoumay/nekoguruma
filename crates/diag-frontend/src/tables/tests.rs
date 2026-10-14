use std::path::PathBuf;

use super::*;

fn example_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/example-ecu")
}

/// The example definition's files, by name.
fn example() -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(example_dir())
        .expect("example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            (name, std::fs::read(entry.path()).expect("read"))
        })
        .collect();
    files.sort();
    files
}

/// The example with `file` replaced by `contents` (added if absent).
fn with(file: &str, contents: &str) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = example().into_iter().filter(|(n, _)| n != file).collect();
    files.push((file.to_owned(), contents.as_bytes().to_vec()));
    files
}

fn without(file: &str) -> Vec<(String, Vec<u8>)> {
    example().into_iter().filter(|(n, _)| n != file).collect()
}

/// The errors as (code, file, line, column).
fn errors(files: Vec<(String, Vec<u8>)>) -> Vec<(ErrorCode, String, Option<u32>, Option<String>)> {
    read_files(files)
        .err()
        .map_or_else(Vec::new, |rejected| rejected.errors)
        .into_iter()
        .map(|e| (e.code, e.file, e.line, e.column))
        .collect()
}

fn at(
    code: ErrorCode,
    file: &str,
    line: Option<u32>,
    column: Option<&str>,
) -> (ErrorCode, String, Option<u32>, Option<String>) {
    (code, file.to_owned(), line, column.map(str::to_owned))
}

/// Errors of one table with `header` and `rows`, the rest of the example unchanged.
fn table_errors(
    file: &str,
    header: &str,
    rows: &[&str],
) -> Vec<(ErrorCode, Option<u32>, Option<String>)> {
    let mut contents = format!("{header}\n");
    for row in rows {
        contents.push_str(row);
        contents.push('\n');
    }
    errors(with(file, &contents))
        .into_iter()
        .map(|(code, f, line, column)| {
            assert_eq!(f, file, "an error in another file");
            (code, line, column)
        })
        .collect()
}

fn cell(code: ErrorCode, line: u32, column: &str) -> (ErrorCode, Option<u32>, Option<String>) {
    (code, Some(line), Some(column.to_owned()))
}

use ErrorCode::*;

// ---------------------------------------------------------------- The example

#[test]
fn the_example_definition_reads_from_its_directory() {
    let tables = read_directory(&example_dir()).expect("the example is valid");
    assert_eq!(
        tables
            .variants
            .iter()
            .map(|v| (v.id, v.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "exampleEcuV1"), (2, "exampleEcuV2")]
    );
    assert_eq!(tables.variants[0].sw_versions, vec!["1.0.0", "1.0.1"]);
    let protocol = &tables.protocols[0];
    assert_eq!(
        (
            protocol.tx_id,
            protocol.rx_id,
            protocol.functional_id,
            protocol.extended_addressing
        ),
        (0x7E0, 0x7E8, 0x7DF, false)
    );
    assert_eq!(tables.texts.len(), 13);
    assert_eq!(tables.fields.len(), 41);
    let session = tables
        .conversions
        .iter()
        .find(|c| c.name == "sessionName")
        .expect("sessionName");
    assert_eq!(
        session.kind,
        ConversionKind::TextTable {
            default_text: Some("value.unknown".into())
        }
    );
    let erase = tables
        .routines
        .iter()
        .find(|r| r.name == "eraseMemory")
        .expect("eraseMemory");
    assert_eq!(
        (
            erase.rid,
            erase.start,
            erase.stop,
            erase.session,
            erase.timing.p2_star_ms,
            erase.timing.p2_ms
        ),
        (0xFF00, true, false, 0x02, 10_000, 50)
    );
    assert_eq!(tables.repeats[0].count, RepeatCount::End);
    assert_eq!(tables.flash_sessions[0].steps.len(), 8);
    assert_eq!(tables.flash_segments[0].block_size, 0x100);
    // A constant stays as written for the field rules.
    let sub = &tables.fields[0];
    assert_eq!(
        (&sub.container, sub.constant.as_deref()),
        (&Container::Request("defaultSession".into()), Some("0x01"))
    );
}

#[test]
fn a_report_serializes_as_the_format_shows() {
    let rejected = read_files(with("variants.csv", "id,name\n1,bad-name\n")).expect_err("rejected");
    let json = serde_json::to_value(&rejected).expect("serializes");
    assert_eq!(json["status"], "rejected");
    assert_eq!(json["errors"][0]["code"], "CSV_BAD_VALUE");
    assert_eq!(json["errors"][0]["file"], "variants.csv");
    assert_eq!(json["errors"][0]["line"], 2);
    assert_eq!(json["errors"][0]["column"], "name");
    // A whole-file error has no line or column.
    let missing = read_files(without("protocols.csv")).expect_err("rejected");
    let json = serde_json::to_value(&missing).expect("serializes");
    assert!(json["errors"][0].get("line").is_none());
    assert!(json["errors"][0].get("column").is_none());
}

// ---------------------------------------------------------------- Section 1: files

#[test]
fn files_outside_the_table_set_are_rejected_or_ignored() {
    let mut files = example();
    files.push(("extra.csv".into(), b"a\n1\n".to_vec()));
    files.push(("Variants.CSV".into(), Vec::new()));
    files.push(("bad name.star".into(), Vec::new()));
    files.push(("README".into(), b"anything, \"unbalanced".to_vec()));
    files.push(("main.star".into(), b"def main(): pass".to_vec()));
    files.push(("café.txt".into(), Vec::new()));
    assert_eq!(
        errors(files),
        vec![
            at(UnknownFile, "Variants.CSV", None, None),
            at(UnknownFile, "bad name.star", None, None),
            at(UnknownFile, "extra.csv", None, None),
        ]
    );
}

#[test]
fn required_tables_must_exist_and_variants_needs_a_row() {
    assert_eq!(
        errors(
            without("variants.csv")
                .into_iter()
                .filter(|(n, _)| n != "protocols.csv")
                .collect()
        ),
        vec![
            at(UnknownFile, "protocols.csv", None, None),
            at(UnknownFile, "variants.csv", None, None),
        ]
    );
    // A header without rows, or a file of zero bytes, is an empty table.
    assert_eq!(
        errors(with("variants.csv", "id,name\n")),
        vec![at(MissingColumn, "variants.csv", None, None)]
    );
    assert_eq!(
        errors(with("variants.csv", "")),
        vec![at(MissingColumn, "variants.csv", None, None)]
    );
    // An optional table may be left out.
    assert!(read_files(without("nrcs.csv")).is_ok());
    assert!(read_files(with("nrcs.csv", "")).is_ok());
}

// ---------------------------------------------------------------- Section 2: dialect

#[test]
fn the_dialect_accepts_crlf_a_bom_quotes_and_blank_rows() {
    let tables = read_files(with(
        "variants.csv",
        "\u{FEFF}name,id,note\r\n\r\n\"exampleEcuV1\",1,\"a, \"\"quoted\"\"\nnote\"\r\n,,\r\nexampleEcuV2,2,\n",
    ))
    .expect("valid");
    assert_eq!(
        tables
            .variants
            .iter()
            .map(|v| (v.line, v.id))
            .collect::<Vec<_>>(),
        vec![(3, 1), (6, 2)]
    );
}

#[test]
fn syntax_errors_stop_the_file_and_name_the_line() {
    assert_eq!(
        table_errors("units.csv", "key,id,symbol", &["volt,1,\"V", "x,2,y"]),
        vec![(Syntax, Some(2), None)]
    );
    let mut files = without("units.csv");
    files.push((
        "units.csv".into(),
        b"key,id,symbol\nvolt,1,V\xFF\n".to_vec(),
    ));
    assert_eq!(errors(files), vec![at(Syntax, "units.csv", Some(2), None)]);
    // A row with the wrong number of cells is reported, and the other rows are read.
    assert_eq!(
        table_errors(
            "units.csv",
            "key,id,symbol",
            &["volt,1", "amp,2,A,x", "ohm,3,R"]
        ),
        vec![(Syntax, Some(2), None), (Syntax, Some(3), None)]
    );
}

#[test]
fn headers_are_checked_by_name() {
    assert_eq!(
        table_errors("units.csv", "id,key,colour,id", &["1,volt,red,1"]),
        vec![
            cell(UnknownColumn, 1, "colour"),
            cell(UnknownColumn, 1, "id")
        ]
    );
    assert_eq!(
        table_errors("texts.csv", "key,text", &["a,b"]),
        vec![cell(MissingColumn, 1, "id")]
    );
    // A required column missing from the header is reported once, not on every row.
    assert_eq!(
        table_errors("texts.csv", "key,text", &["a,b", "c,d"]).len(),
        1
    );
    // `variants` exists only in scoped tables; `note` in every table.
    assert_eq!(
        table_errors("units.csv", "key,id,variants,note", &["volt,1,,x"]),
        vec![cell(UnknownColumn, 1, "variants")]
    );
}

#[test]
fn cells_are_taken_as_written() {
    assert_eq!(
        table_errors("units.csv", "key,id,symbol", &["volt, 1,V", "amp,,A"]),
        vec![cell(BadValue, 2, "id"), cell(MissingColumn, 3, "id")]
    );
}

// ---------------------------------------------------------------- Sections 3 and 4: values

#[test]
fn ranges_follow_each_column() {
    let protocol = |row: &str| {
        table_errors(
            "protocols.csv",
            "name,baud_rate,tx_id,rx_id,functional_id",
            &[row],
        )
    };
    assert_eq!(
        protocol("CAN,0,0x7E0,0x7E8,"),
        vec![cell(BadValue, 2, "baud_rate")]
    );
    assert_eq!(
        protocol("CAN,500000,0x800,0x7E8,"),
        vec![cell(BadValue, 2, "tx_id")]
    );
    assert_eq!(
        protocol("CAN,500000,0x7E0,0x7E8,0X7DF"),
        vec![cell(BadValue, 2, "functional_id")]
    );
    let service = |sid: &str| table_errors("services.csv", "name,sid", &[&format!("s,{sid}")]);
    assert_eq!(service("0x3F"), vec![cell(BadValue, 2, "sid")]);
    assert_eq!(service("0xC0"), vec![cell(BadValue, 2, "sid")]);
    assert!(read_files(with("services.csv", "name,sid,p2_ms\ns,0xBF,1\n")).is_ok());
    let comparam = |value: &str| {
        table_errors(
            "comparams.csv",
            "protocol,name,value",
            &[&format!("ISO15765,CP,{value}")],
        )
    };
    assert!(comparam("-9223372036854775808").is_empty());
    assert_eq!(
        comparam("0x8000000000000000"),
        vec![cell(BadValue, 2, "value")]
    );
}

#[test]
fn lists_names_keys_and_service_names_are_checked() {
    assert_eq!(
        table_errors(
            "variants.csv",
            "id,name,sw_versions",
            &["1,v1,1.0;1.0", "2,v2,1.0;", "3,v.3,"]
        ),
        vec![
            cell(BadValue, 2, "sw_versions"),
            cell(BadValue, 3, "sw_versions"),
            cell(BadValue, 4, "name"),
        ]
    );
    assert_eq!(
        table_errors(
            "nrcs.csv",
            "service,nrc,text",
            &["vin.erase,0x22,", "vin.read,0x00,", ",0x22,a..b"]
        ),
        vec![
            cell(BadValue, 2, "service"),
            cell(BadValue, 3, "nrc"),
            cell(BadValue, 4, "text"),
        ]
    );
    assert_eq!(
        table_errors("dids.csv", "variants,name,did", &["v1;v1,x,0x0100"]),
        vec![cell(BadValue, 2, "variants")]
    );
}

#[test]
fn a_conversion_uses_only_the_cells_of_its_kind() {
    let header =
        "name,kind,numerator,denominator,offset,numerator_coeffs,denominator_coeffs,default_text";
    assert_eq!(
        table_errors(
            "conversions.csv",
            header,
            &[
                "a,linear,2,0,,,,",
                "b,linear,,,,1,,",
                "c,rational,,,,1,0;0,",
                "d,rational,,,,,1,",
                "e,identity,,,,,,text.x",
                "f,text-table,1,,,,,",
                "g,cubic,,,,,,",
                "h,scale-linear,,,1e999,,,",
            ]
        ),
        vec![
            cell(BadValue, 2, "denominator"),
            cell(BadValue, 3, "numerator_coeffs"),
            cell(BadValue, 4, "denominator_coeffs"),
            cell(MissingColumn, 5, "numerator_coeffs"),
            cell(BadValue, 6, "default_text"),
            cell(BadValue, 7, "numerator"),
            cell(BadValue, 8, "kind"),
            cell(BadValue, 9, "offset"),
        ]
    );
    let tables = read_files(with(
        "conversions.csv",
        &format!("{header}\nsessionName,text-table,,,,,,\nvoltage,linear,1,10,-0.5,,,\nr,rational,,,,1;2,3,\nresetDetails,identity,,,,,,\nsupplyVoltage,identity,,,,,,\n"),
    ))
    .expect("valid");
    assert_eq!(
        tables.conversions[1].kind,
        ConversionKind::Linear {
            numerator: 1.0,
            denominator: 10.0,
            offset: -0.5
        }
    );
    assert_eq!(
        tables.conversions[2].kind,
        ConversionKind::Rational {
            numerator_coeffs: vec![1.0, 2.0],
            denominator_coeffs: vec![3.0]
        }
    );
}

#[test]
fn rows_that_generate_no_service_are_rejected() {
    assert_eq!(
        table_errors("dids.csv", "name,did,read,write", &["x,0x0100,no,no"]),
        vec![cell(MissingColumn, 2, "read")]
    );
    assert_eq!(
        table_errors("routines.csv", "name,rid,start", &["x,0x0100,no"]),
        vec![cell(MissingColumn, 2, "start")]
    );
}

#[test]
fn flash_rows_check_steps_and_address_ranges() {
    assert_eq!(
        table_errors(
            "flash_sessions.csv",
            "id,name,steps",
            &[
                "1,application,erase-memory;erase-memory",
                "2,boot,format-disk"
            ]
        ),
        vec![cell(BadValue, 2, "steps"), cell(BadValue, 3, "steps")]
    );
    assert_eq!(
        table_errors(
            "flash_segments.csv",
            "session,name,start_address,length,artifact_offset,block_size",
            &[
                "application,app,0xFFFFFFFFFFFFFFFF,1,0,1",
                "application,b,0xFFFFFFFFFFFFFFFF,0,0,1",
                "application,c,0xFFFFFFFFFFFFFFFE,2,0xFFFFFFFFFFFFFFFF,1",
            ]
        ),
        vec![
            cell(BadValue, 3, "length"),
            cell(BadValue, 4, "artifact_offset"),
        ]
    );
}

// ---------------------------------------------------------------- Section 5: field tables

#[test]
fn field_cells_are_checked_for_their_own_syntax() {
    let header = "container,name,byte,bit,type,bits,shape";
    assert_eq!(
        table_errors(
            "fields.csv",
            header,
            &[
                "request:x:y,a,,,uint,8,",
                "routine:r:begin:request,a,,,uint,8,",
                "did:x,a,536870912,,uint,8,",
                "did:x,b,,8,uint,8,",
                "did:x,c,,,,8,",
                "did:x,d,,,word,8,",
                "did:x,e,,,,,group",
                "did:x,did,,,uint,8,",
                "routine:r:start:response,rid,,,uint,8,",
            ]
        ),
        vec![
            cell(BadValue, 2, "container"),
            cell(BadValue, 3, "container"),
            cell(BadValue, 4, "byte"),
            cell(BadValue, 5, "bit"),
            cell(MissingColumn, 6, "type"),
            cell(BadValue, 7, "type"),
            cell(Duplicate, 9, "name"),
            cell(Duplicate, 10, "name"),
        ]
    );
}

#[test]
fn a_repeat_count_uses_only_the_cells_of_its_kind() {
    let header = "container,field,count,items,item_bytes,marker_bits,marker_lower,marker_upper";
    let rows = [
        "layout:a,f1,fixed,2,,,,",
        "layout:a,f2,fixed,2,4,8,,",
        "layout:a,f3,marker,,,12,0,",
        "layout:a,f4,marker,,,8,0x100,",
        "layout:a,f5,marker,,,8,5,4",
        "layout:a,f6,marker,,,64,0x7FFFFFFFFFFFFFFF,0x8000000000000000",
        "layout:a,f7,end,1,,,,",
        "layout:a,f8,marker,,,64,0xFFFFFFFFFFFFFFFF,",
        "layout:a,f9,field,,,,,",
    ];
    assert_eq!(
        table_errors("repeats.csv", header, &rows),
        vec![
            cell(MissingColumn, 2, "item_bytes"),
            cell(BadValue, 3, "marker_bits"),
            cell(BadValue, 4, "marker_bits"),
            cell(BadValue, 5, "marker_lower"),
            cell(BadValue, 6, "marker_upper"),
            cell(BadValue, 7, "marker_upper"),
            cell(BadValue, 8, "items"),
        ]
    );
}

#[test]
fn select_case_limits_take_the_widest_key_range() {
    let header = "container,field,case,lower,upper,layout";
    assert_eq!(
        table_errors(
            "select_cases.csv",
            header,
            &[
                "did:x,k,a,-9223372036854775808,0xFFFFFFFFFFFFFFFF,",
                "did:x,k,b,,5,",
                "did:x,k,c,5,4,",
                "did:x,k,d,0x10000000000000000,,",
                "did:x,k,e,,,",
                "did:x,k,f,,abc,",
            ]
        ),
        vec![
            cell(MissingColumn, 3, "lower"),
            cell(BadValue, 4, "upper"),
            cell(BadValue, 5, "lower"),
            cell(MissingColumn, 7, "lower"),
            cell(BadValue, 7, "upper"),
        ]
    );
}

// ---------------------------------------------------------------- Duplicates and order

#[test]
fn names_and_ids_defined_twice_in_a_table_are_rejected() {
    assert_eq!(
        table_errors("variants.csv", "id,name", &["1,a", "1,b", "2,a"]),
        vec![cell(Duplicate, 3, "id"), cell(Duplicate, 4, "name")]
    );
    assert_eq!(
        table_errors(
            "select_cases.csv",
            "container,field,case,lower",
            &["did:x,k,a,1", "did:x,k,a,2", "did:y,k,a,2", "did:x,j,a,3"]
        ),
        vec![cell(Duplicate, 3, "case")]
    );
    assert_eq!(
        table_errors(
            "flash_segments.csv",
            "session,name,start_address,length,artifact_offset,block_size",
            &["s,a,0,1,0,1", "s,a,1,1,1,1", "t,a,2,1,2,1"]
        ),
        vec![cell(Duplicate, 3, "name")]
    );
}

#[test]
fn every_error_is_listed_sorted_by_file_line_and_column() {
    let mut files = with("units.csv", "key,id,symbol\nvolt,x,V\n");
    files.retain(|(n, _)| n != "variants.csv");
    files.push(("variants.csv".into(), b"name,id\nbad-name,0\n".to_vec()));
    files.push(("aaa.csv".into(), Vec::new()));
    assert_eq!(
        errors(files),
        vec![
            at(UnknownFile, "aaa.csv", None, None),
            at(BadValue, "units.csv", Some(2), Some("id")),
            at(BadValue, "variants.csv", Some(2), Some("id")),
            at(BadValue, "variants.csv", Some(2), Some("name")),
        ]
    );
}

// ---------------------------------------------------------------- Edge cases

#[test]
fn a_column_one_kind_of_row_requires_is_reported_when_the_header_leaves_it_out() {
    assert_eq!(
        table_errors(
            "conversions.csv",
            "name,kind",
            &["r,rational", "i,identity"]
        ),
        vec![
            cell(MissingColumn, 2, "denominator_coeffs"),
            cell(MissingColumn, 2, "numerator_coeffs"),
        ]
    );
    assert_eq!(
        table_errors(
            "repeats.csv",
            "container,field,count",
            &["layout:a,f,fixed", "layout:a,g,marker", "layout:a,h,end"]
        ),
        vec![
            cell(MissingColumn, 2, "item_bytes"),
            cell(MissingColumn, 2, "items"),
            cell(MissingColumn, 3, "marker_bits"),
            cell(MissingColumn, 3, "marker_lower"),
        ]
    );
}

#[test]
fn a_file_that_breaks_the_record_format_reports_nothing_else() {
    assert_eq!(
        errors(with("variants.csv", "id,name\n1,exampleEcuV1\n2,\"x\"y\n")),
        vec![at(Syntax, "variants.csv", Some(3), None)]
    );
    // Rows with the wrong number of cells are syntax errors too, not an empty table.
    assert_eq!(
        errors(with("variants.csv", "id,name\n1\n2,b,c\n")),
        vec![
            at(Syntax, "variants.csv", Some(2), None),
            at(Syntax, "variants.csv", Some(3), None),
        ]
    );
}

#[test]
fn one_bad_cell_does_not_hide_the_others_of_its_row() {
    let header = "container,field,count,marker_bits,marker_lower,marker_upper";
    assert_eq!(
        table_errors("repeats.csv", header, &["layout:a,f,marker,12,zz,qq"]),
        vec![
            cell(BadValue, 2, "marker_bits"),
            cell(BadValue, 2, "marker_lower"),
            cell(BadValue, 2, "marker_upper"),
        ]
    );
    // A wrong width does not hide limits in the wrong order.
    assert_eq!(
        table_errors("repeats.csv", header, &["layout:a,f,marker,12,5,4"]),
        vec![
            cell(BadValue, 2, "marker_bits"),
            cell(BadValue, 2, "marker_upper"),
        ]
    );
    assert_eq!(
        table_errors("repeats.csv", header, &["layout:a,f,forever,8,zz,"]),
        vec![
            cell(BadValue, 2, "count"),
            cell(BadValue, 2, "marker_lower")
        ]
    );
    assert_eq!(
        table_errors("conversions.csv", "name,kind,numerator", &["r,cubic,abc"]),
        vec![cell(BadValue, 2, "kind"), cell(BadValue, 2, "numerator")]
    );
    // A row with another error is still checked for a duplicate key, compared by value.
    assert_eq!(
        table_errors(
            "variants.csv",
            "id,name",
            &["1,exampleEcuV1", "0x01,bad-name"]
        ),
        vec![cell(Duplicate, 3, "id"), cell(BadValue, 3, "name")]
    );
}

#[test]
fn a_repeated_header_column_keeps_its_first_occurrence() {
    assert_eq!(
        table_errors("variants.csv", "id,name,id", &["1,exampleEcuV1,zz"]),
        vec![cell(UnknownColumn, 1, "id")]
    );
}

#[cfg(unix)]
#[test]
fn a_directory_follows_links_and_rejects_entries_that_are_not_files() {
    let dir = std::env::temp_dir().join(format!(
        "ngr-csv-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir(&dir).expect("temporary directory");
    for (name, contents) in example() {
        if name != "dids.csv" {
            std::fs::write(dir.join(&name), contents).expect("write");
        }
    }
    std::os::unix::fs::symlink(example_dir().join("dids.csv"), dir.join("dids.csv")).expect("link");
    let tables = read_directory(&dir).expect("valid through the link");
    assert_eq!(tables.dids.len(), 8);
    std::fs::create_dir(dir.join("odd.csv")).expect("directory");
    std::os::unix::fs::symlink(dir.join("missing"), dir.join("broken.star")).expect("link");
    let result = read_directory(&dir);
    std::fs::remove_dir_all(&dir).expect("clean up");
    let Err(ReadError::Rejected(rejected)) = result else {
        panic!("rejected: {result:?}");
    };
    assert_eq!(
        rejected
            .errors
            .iter()
            .map(|e| (e.code, e.file.as_str()))
            .collect::<Vec<_>>(),
        vec![(UnknownFile, "broken.star"), (UnknownFile, "odd.csv")]
    );
}

#[test]
fn an_empty_length_is_kept_apart_from_an_explicit_one() {
    let tables = read_files(with(
        "fields.csv",
        "container,name,shape,layout,length\nlayout:x,a,group,y,\nlayout:x,b,group,y,fixed\n",
    ))
    .expect("cell syntax is valid; the field rules come later");
    assert_eq!(
        tables.fields.iter().map(|f| f.length).collect::<Vec<_>>(),
        vec![None, Some(Length::Fixed)]
    );
}

#[cfg(unix)]
#[test]
fn entries_that_are_not_definition_files_are_never_looked_at() {
    let dir = std::env::temp_dir().join(format!(
        "ngr-csv-loop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir(&dir).expect("temporary directory");
    for (name, contents) in example() {
        std::fs::write(dir.join(&name), contents).expect("write");
    }
    // A link to itself cannot be followed; as a README it is simply not read.
    std::os::unix::fs::symlink(dir.join("README"), dir.join("README")).expect("link");
    let ignored = read_directory(&dir).map(|t| t.variants.len());
    // Named like a table, the same loop is rejected rather than failing the read.
    std::os::unix::fs::symlink(dir.join("loop.csv"), dir.join("loop.csv")).expect("link");
    let rejected = read_directory(&dir);
    std::fs::remove_dir_all(&dir).expect("clean up");
    assert_eq!(ignored.expect("valid"), 2);
    let Err(ReadError::Rejected(rejected)) = rejected else {
        panic!("rejected: {rejected:?}");
    };
    assert_eq!(rejected.errors.len(), 1);
    assert_eq!(
        (rejected.errors[0].code, rejected.errors[0].file.as_str()),
        (UnknownFile, "loop.csv")
    );
}

#[test]
fn a_file_given_twice_is_reported_and_its_first_copy_read() {
    let mut files = with("variants.csv", "id,name\n1,bad-name\n");
    files.push(("variants.csv".into(), b"id,name\n2,valid\n".to_vec()));
    assert_eq!(
        errors(files),
        vec![
            at(Duplicate, "variants.csv", None, None),
            at(BadValue, "variants.csv", Some(2), Some("name")),
        ]
    );
}
