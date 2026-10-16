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

// ---------------------------------------------------------------- Resolving

/// The errors of resolving `files`, which must read cleanly, as (code, file, line, column).
fn resolved(
    files: Vec<(String, Vec<u8>)>,
) -> Vec<(ErrorCode, String, Option<u32>, Option<String>)> {
    let tables = read_files(files).expect("the tables read");
    resolve(tables)
        .err()
        .map_or_else(Vec::new, |rejected| rejected.errors)
        .into_iter()
        .map(|e| (e.code, e.file, e.line, e.column))
        .collect()
}

/// The resolve errors of one table replaced by `header` and `rows`, as (code, file, line,
/// column).
fn resolved_with(
    file: &str,
    header: &str,
    rows: &[&str],
) -> Vec<(ErrorCode, String, Option<u32>, Option<String>)> {
    let mut contents = format!("{header}\n");
    for row in rows {
        contents.push_str(row);
        contents.push('\n');
    }
    resolved(with(file, &contents))
}

#[test]
fn the_example_definition_resolves_into_two_variants() {
    let definition = resolve(read_files(example()).expect("reads")).expect("resolves");
    assert_eq!(definition.variants.len(), 2);
    let names = |v: &VariantRows| -> Vec<&str> {
        v.dids
            .iter()
            .map(|&i| definition.tables.dids[i].name.as_str())
            .collect()
    };
    // supplyVoltage is scoped to the second variant; every other DID is in both.
    assert!(!names(&definition.variants[0]).contains(&"supplyVoltage"));
    assert!(names(&definition.variants[1]).contains(&"supplyVoltage"));
    assert_eq!(names(&definition.variants[0]).len(), 7);
    assert_eq!(names(&definition.variants[1]).len(), 8);
    for variant in &definition.variants {
        assert_eq!(variant.protocols, vec![0]);
        assert_eq!(variant.comparams, vec![0, 1]);
        assert_eq!(variant.routines, vec![0, 1]);
        assert_eq!(variant.dtcs, vec![0, 1]);
        assert_eq!(variant.services.len(), 4);
        assert_eq!(variant.nrcs.len(), 5);
        assert_eq!(variant.flash_sessions, vec![0]);
    }
}

#[test]
fn an_unknown_variant_in_a_scope_is_reported() {
    assert_eq!(
        resolved_with(
            "dtcs.csv",
            "variants,code,display,text,status",
            &[
                "exampleEcuV1;exampleEcuV3,0x910116,B1101-16,,dtcStatus",
                ",0xC10000,U0100-00,,dtcStatus",
            ],
        ),
        vec![at(UnknownReference, "dtcs.csv", Some(2), Some("variants"))]
    );
}

#[test]
fn every_variant_needs_a_protocol_and_comparams_follow_their_protocol() {
    assert_eq!(
        resolved_with(
            "protocols.csv",
            "variants,name,baud_rate,tx_id,rx_id",
            &["exampleEcuV1,ISO15765,500000,0x7E0,0x7E8"],
        ),
        vec![
            at(UnknownReference, "comparams.csv", Some(2), Some("protocol")),
            at(UnknownReference, "comparams.csv", Some(3), Some("protocol")),
            at(MissingColumn, "variants.csv", Some(3), Some("name")),
        ]
    );
    assert_eq!(
        resolved_with(
            "comparams.csv",
            "protocol,name,value,unit",
            &["ISO15765,CP_StMin,0,millisecond", "ISO15765,CP_Bs,0,parsec"],
        ),
        vec![at(UnknownReference, "comparams.csv", Some(3), Some("unit"))]
    );
}

#[test]
fn variant_patterns_name_a_value_of_a_response_in_their_variant() {
    const P: &str = "variant_patterns.csv";
    assert_eq!(
        resolved_with(
            P,
            "variant,service,field,expected",
            &[
                "exampleEcuV1,softwareVersion.read,softwareVersion,31",
                "exampleEcuV3,softwareVersion.read,softwareVersion,31",
                "exampleEcuV1,softwareVersion.write,softwareVersion,31",
                "exampleEcuV1,supplyVoltage.read,supplyVoltage,00",
                "exampleEcuV1,defaultSession,sub,01",
                "exampleEcuV1,readDtcByStatusMask,records,01",
                "exampleEcuV1,defaultSession,missing,01",
                "exampleEcuV1,checkProgrammingDependencies.start,result,00",
                "exampleEcuV1,vin.write,vin,31",
            ],
        ),
        vec![
            at(UnknownReference, P, Some(3), Some("variant")),
            at(UnknownReference, P, Some(4), Some("service")),
            // The supply voltage's linear conversion makes it a float.
            at(BadValue, P, Some(5), Some("field")),
            at(UnknownReference, P, Some(5), Some("service")),
            // A constant has no IR field; a shaped field is not a scalar value.
            at(UnknownReference, P, Some(6), Some("field")),
            at(BadValue, P, Some(7), Some("field")),
            at(UnknownReference, P, Some(8), Some("field")),
            // A write's positive response carries only the identifier.
            at(UnknownReference, P, Some(10), Some("field")),
        ]
    );
}

#[test]
fn variant_patterns_compare_only_values_they_can_match_exactly() {
    // `powerCycles` is made a BCD field and `blockNumber` a float, for the test.
    const P: &str = "variant_patterns.csv";
    let fields = example_text("fields.csv")
        .replace(
            "did:powerCycles,powerCycles,0,,uint,32,",
            "did:powerCycles,powerCycles,0,,bcd,32,",
        )
        .replace(
            "did:flashState,blockNumber,1,,uint,32,",
            "did:flashState,blockNumber,1,,float,32,",
        )
        .replace("did:vin,vin,0,,ascii,136,", "did:vin,vin,0,,bytes,16,")
        .replace(
            "did:flashState,phase,0,,uint,8,",
            "did:flashState,phase,0,,int,8,",
        );
    let mut files = with("fields.csv", &fields);
    files.retain(|(n, _)| n != P);
    files.push((
        P.into(),
        b"variant,service,field,expected
exampleEcuV2,supplyVoltage.read,supplyVoltage,0078
exampleEcuV1,flashState.read,blockNumber,00
exampleEcuV1,flashState.read,bytesReceived,00 00 00 00 00 00 00 00 01
exampleEcuV1,powerCycles.read,powerCycles,1A
exampleEcuV1,powerCycles.read,powerCycles,12345678
exampleEcuV1,activeSession.read,session,01
exampleEcuV1,flashState.read,phase,00 00 00 00 00 00 00 01
exampleEcuV1,powerCycles.read,powerCycles,00 12 34 56 78
exampleEcuV1,powerCycles.read,powerCycles,01 23 45 67 89
exampleEcuV1,flashState.read,bytesReceived,00 00 00 01 00 00 00 00
exampleEcuV1,flashState.read,phase,FF 80
exampleEcuV1,flashState.read,phase,00 FE
exampleEcuV1,vin.read,vin,31 32
exampleEcuV1,vin.read,vin,31 32 33
exampleEcuV1,softwareVersion.read,softwareVersion,FF
"
        .to_vec(),
    ));
    assert_eq!(
        resolved(files),
        vec![
            // A numeric conversion, and a float field, give floats.
            at(BadValue, P, Some(2), Some("field")),
            at(BadValue, P, Some(3), Some("field")),
            // An integer takes at most eight bytes; BCD digits are 0 to 9.
            at(BadValue, P, Some(4), Some("expected")),
            at(BadValue, P, Some(5), Some("expected")),
            // Leading zeros are fine, but not a value the fixed width cannot hold: nine BCD
            // digits in 32 bits, 2^32 in 32 bits, 254 in a signed byte, three bytes in two.
            at(BadValue, P, Some(10), Some("expected")),
            at(BadValue, P, Some(11), Some("expected")),
            at(BadValue, P, Some(13), Some("expected")),
            at(BadValue, P, Some(15), Some("expected")),
            // Text is compared in UTF-8.
            at(BadValue, P, Some(16), Some("expected")),
        ]
    );
}

#[test]
fn nrcs_name_a_service_in_each_of_their_variants_and_a_text() {
    assert_eq!(
        resolved_with(
            "nrcs.csv",
            "variants,service,nrc,text",
            &[
                ",unknownService,0x22,",
                ",supplyVoltage.read,0x22,",
                "exampleEcuV2,supplyVoltage.read,0x31,",
                ",,0x7F,nrc.missing",
            ],
        ),
        vec![
            at(UnknownReference, "nrcs.csv", Some(2), Some("service")),
            at(UnknownReference, "nrcs.csv", Some(3), Some("service")),
            at(UnknownReference, "nrcs.csv", Some(5), Some("text")),
        ]
    );
}

#[test]
fn conversion_texts_and_scales_follow_the_conversion_kind() {
    let mut files = with(
        "conversions.csv",
        "name,kind,numerator,denominator,offset,default_text\n\
         sessionName,text-table,,,,value.missing\n\
         checkResult,text-table,,,,\n\
         supplyVoltage,linear,1,10,0,\n\
         tenMilliseconds,linear,10,1,0,\n\
         bands,scale-linear,,,,\n\
         curve,interpolation,,,,\n\
         empty,scale-linear,,,,\n\
         short,interpolation,,,,\n",
    );
    files.retain(|(n, _)| n != "conversion_scales.csv");
    files.push((
        "conversion_scales.csv".into(),
        b"conversion,lower,upper,numerator,denominator,offset,text,internal,physical\n\
          sessionName,0x01,,,,,session.default,,\n\
          checkResult,0x00,,,,,,,\n\
          checkResult,0x02,0x01,,,,check.correct,,\n\
          checkResult,0x03,,,,,check.missing,,\n\
          supplyVoltage,,,,,,,,\n\
          missing,,,,,,,,\n\
          bands,0,10,1,1,0,,,\n\
          bands,10.5,20,1,1,0,check.correct,,\n\
          bands,15,30,1,1,0,,,\n\
          bands,,,1,1,0,,,\n\
          curve,,,,,,,0,0\n\
          curve,,,,,,,1,10\n\
          curve,,,,,,,1,20\n\
          curve,,,,,,,,30\n\
          short,,,,,,,0,0\n"
            .to_vec(),
    ));
    const S: &str = "conversion_scales.csv";
    assert_eq!(
        resolved(files),
        vec![
            at(MissingColumn, S, Some(3), Some("text")),
            at(BadValue, S, Some(4), Some("upper")),
            at(UnknownReference, S, Some(5), Some("text")),
            at(BadValue, S, Some(6), Some("conversion")),
            at(UnknownReference, S, Some(7), Some("conversion")),
            at(BadValue, S, Some(9), Some("text")),
            at(BadValue, S, Some(10), Some("lower")),
            at(MissingColumn, S, Some(11), Some("lower")),
            at(BadValue, S, Some(14), Some("internal")),
            at(MissingColumn, S, Some(15), Some("internal")),
            at(
                UnknownReference,
                "conversions.csv",
                Some(2),
                Some("default_text")
            ),
            at(MissingColumn, "conversions.csv", Some(8), Some("kind")),
            at(MissingColumn, "conversions.csv", Some(9), Some("kind")),
        ]
    );
}

#[test]
fn dtcs_name_existing_texts_and_layouts() {
    assert_eq!(
        resolved_with(
            "dtcs.csv",
            "code,display,text,status,snapshot,extended",
            &["0x910116,B1101-16,dtc.missing,noStatus,noSnapshot,dtcRecord"],
        ),
        vec![
            at(UnknownReference, "dtcs.csv", Some(2), Some("snapshot")),
            at(UnknownReference, "dtcs.csv", Some(2), Some("status")),
            at(UnknownReference, "dtcs.csv", Some(2), Some("text")),
        ]
    );
}

#[test]
fn flash_segments_belong_to_a_session_and_do_not_overlap() {
    let mut files = with(
        "flash_segments.csv",
        "session,name,start_address,length,artifact_offset,block_size\n\
         application,app,0x0000,0x1000,0,0x100\n\
         application,cal,0x0FFF,0x1,0,0x100\n\
         application,tail,0x1000,0x10,0,0x100\n\
         bootloader,boot,0x0000,0x10,0,0x100\n",
    );
    files.retain(|(n, _)| n != "flash_sessions.csv");
    files.push((
        "flash_sessions.csv".into(),
        b"id,name,steps\n1,application,ecu-reset\n2,calibration,ecu-reset\n".to_vec(),
    ));
    assert_eq!(
        resolved(files),
        vec![
            at(
                Duplicate,
                "flash_segments.csv",
                Some(3),
                Some("start_address")
            ),
            at(
                UnknownReference,
                "flash_segments.csv",
                Some(5),
                Some("session")
            ),
            at(MissingColumn, "flash_sessions.csv", Some(3), Some("name")),
        ]
    );
}

#[test]
fn fields_name_existing_owners_and_references() {
    let mut contents = String::from_utf8(
        example()
            .into_iter()
            .find(|(n, _)| n == "fields.csv")
            .unwrap()
            .1,
    )
    .expect("UTF-8");
    contents.push_str(
        "response:unknownService,x,0,,uint,8,,,,,,,,,\n\
         did:unknownDid,x,0,,uint,8,,,,,,,,,\n\
         routine:unknownRoutine:start:request,x,0,,uint,8,,,,,,,,,\n\
         routine:eraseMemory:stop:request,x,0,,uint,8,,,,,,,,,\n\
         layout:unknownLayout,x,0,,uint,8,,,,,,,,,\n\
         layout:dtcRecord,converted,4,,uint,8,,,,noConversion,parsec,,,,\n\
         layout:dtcRecord,nested,5,,,,,,,,,,repeat,noLayout,\n\
         layout:dtcRecord,later,6,,uint,8,,,,,,,,,afterwards\n\
         layout:dtcRecord,afterwards,7,,uint,8,,,,,,,,,\n\
         layout:dtcStatus,elsewhere,1,,uint,8,,,,,,,,,dtc\n\
         layout:dtcStatus,nowhere,1,,uint,8,,,,,,,,,missing\n",
    );
    // Every field of powerCycles goes, so the DID has none.
    let contents: String = contents
        .lines()
        .filter(|l| !l.starts_with("did:powerCycles,"))
        .map(|l| format!("{l}\n"))
        .collect();
    let lines = |prefix: &str| -> u32 {
        let index = contents
            .lines()
            .position(|l| l.starts_with(prefix))
            .unwrap();
        u32::try_from(index).unwrap() + 1
    };
    const F: &str = "fields.csv";
    let e = |code, prefix: &str, column: &str| at(code, F, Some(lines(prefix)), Some(column));
    let expected = vec![
        at(MissingColumn, "dids.csv", Some(7), Some("name")),
        e(UnknownReference, "response:unknownService", "container"),
        e(UnknownReference, "did:unknownDid", "container"),
        e(UnknownReference, "routine:unknownRoutine", "container"),
        e(UnknownReference, "routine:eraseMemory:stop", "container"),
        e(UnknownReference, "layout:unknownLayout", "container"),
        e(UnknownReference, "layout:dtcRecord,converted", "conversion"),
        e(UnknownReference, "layout:dtcRecord,converted", "unit"),
        e(UnknownReference, "layout:dtcRecord,nested", "layout"),
        // A later field of the same container, a field of another one, and no field at all.
        e(BadValue, "layout:dtcRecord,later", "ref"),
        e(Unsupported, "layout:dtcStatus,elsewhere", "ref"),
        e(UnknownReference, "layout:dtcStatus,nowhere", "ref"),
    ];
    assert_eq!(resolved(with(F, &contents)), expected);
}

#[test]
fn repeats_and_cases_name_existing_fields_and_layouts() {
    let mut files = with(
        "repeats.csv",
        "container,field,count\n\
         response:readDtcByStatusMask,records,end\n\
         response:readDtcByStatusMask,missing,end\n",
    );
    files.retain(|(n, _)| n != "select_cases.csv");
    files.push((
        "select_cases.csv".into(),
        b"container,field,case,lower,upper,layout\n\
          did:lastResetReason,details,powerOn,0x01,,\n\
          did:lastResetReason,reason,watchdog,0x02,,noLayout\n"
            .to_vec(),
    ));
    assert_eq!(
        resolved(files),
        vec![
            at(UnknownReference, "repeats.csv", Some(3), Some("field")),
            at(
                UnknownReference,
                "select_cases.csv",
                Some(3),
                Some("layout")
            ),
        ]
    );
}

#[test]
fn a_key_is_unique_within_each_variant_document() {
    let mut dids = String::from_utf8(
        example()
            .into_iter()
            .find(|(n, _)| n == "dids.csv")
            .unwrap()
            .1,
    )
    .expect("UTF-8");
    // The same identifier in two variants is fine; in one variant it is a duplicate.
    dids.push_str("exampleEcuV1,supplyVoltageV1,0xFD02,yes,no,,,\n");
    dids.push_str("exampleEcuV2,vinAgain,0xF190,yes,no,,,\n");
    let mut fields = String::from_utf8(
        example()
            .into_iter()
            .find(|(n, _)| n == "fields.csv")
            .unwrap()
            .1,
    )
    .expect("UTF-8");
    fields.push_str("did:supplyVoltageV1,v,0,,uint,16,,,,,,,,,\n");
    fields.push_str("did:vinAgain,v,0,,uint,8,,,,,,,,,\n");
    let mut files = with("dids.csv", &dids);
    files.retain(|(n, _)| n != "fields.csv");
    files.push(("fields.csv".into(), fields.into_bytes()));
    assert_eq!(
        resolved(files),
        vec![at(Duplicate, "dids.csv", Some(11), Some("did"))]
    );
    assert_eq!(
        resolved_with(
            "nrcs.csv",
            "variants,service,nrc",
            &[
                ",,0x22",
                "exampleEcuV2,,0x22",
                "exampleEcuV1,defaultSession,0x22"
            ],
        ),
        vec![at(Duplicate, "nrcs.csv", Some(3), Some("nrc"))]
    );
}

#[test]
fn a_service_row_replaces_the_variant_wide_row_of_its_nrc() {
    let files = with(
        "nrcs.csv",
        "variants,service,nrc\n\
         ,,0x22\n\
         ,,0x31\n\
         exampleEcuV2,defaultSession,0x31\n\
         ,defaultSession,0x7F\n",
    );
    let definition = resolve(read_files(files).expect("reads")).expect("resolves");
    let service = |name: &str| ServiceName {
        name: name.into(),
        generated: None,
    };
    let nrcs = |variant: usize, name: &str| -> Vec<u8> {
        definition
            .negative_responses(variant, &service(name))
            .into_iter()
            .map(|i| definition.tables.nrcs[i].nrc)
            .collect()
    };
    assert_eq!(nrcs(0, "defaultSession"), vec![0x7F, 0x22, 0x31]);
    assert_eq!(nrcs(1, "defaultSession"), vec![0x31, 0x7F, 0x22]);
    assert_eq!(nrcs(1, "extendedSession"), vec![0x22, 0x31]);
}

/// The example's `file`, as text.
fn example_text(file: &str) -> String {
    let (_, bytes) = example()
        .into_iter()
        .find(|(n, _)| n == file)
        .expect("in the example");
    String::from_utf8(bytes).expect("UTF-8")
}

#[test]
fn an_unknown_variant_is_reported_only_where_it_is_written() {
    // A typo in a protocol's scope does not also fail its ComParams or the variant it missed.
    assert_eq!(
        resolved_with(
            "protocols.csv",
            "variants,name,baud_rate,tx_id,rx_id",
            &["exampleEcuV11;exampleEcuV2,ISO15765,500000,0x7E0,0x7E8"],
        ),
        vec![at(
            UnknownReference,
            "protocols.csv",
            Some(2),
            Some("variants")
        )]
    );
    // Nor does a typo in a DID's scope fail the NRCs and variant patterns that name it.
    let dids = example_text("dids.csv").replace(
        "\n,softwareVersion,",
        "\nexampleEcuV11;exampleEcuV2,softwareVersion,",
    );
    let mut files = with("dids.csv", &dids);
    files.retain(|(n, _)| n != "nrcs.csv");
    files.push((
        "nrcs.csv".into(),
        b"service,nrc\nsoftwareVersion.read,0x31\n".to_vec(),
    ));
    assert_eq!(
        resolved(files),
        vec![at(UnknownReference, "dids.csv", Some(4), Some("variants"))]
    );
}

#[test]
fn a_missing_lower_limit_is_reported_beside_a_bad_upper_one() {
    assert_eq!(
        resolved_with(
            "conversion_scales.csv",
            "conversion,lower,upper,text",
            &[
                "sessionName,0x01,,session.default",
                "checkResult,,abc,check.correct",
            ],
        ),
        vec![
            at(
                MissingColumn,
                "conversion_scales.csv",
                Some(3),
                Some("lower")
            ),
            at(BadValue, "conversion_scales.csv", Some(3), Some("upper")),
        ]
    );
}

#[test]
fn a_layout_a_dtc_names_holds_no_shaped_field() {
    let fields = example_text("fields.csv");
    let line = u32::try_from(fields.lines().count()).unwrap() + 1;
    let fields = format!("{fields}layout:dtcStatus,nested,1,,,,,,,,,,repeat,dtcRecord,\n");
    let mut files = with("fields.csv", &fields);
    files.retain(|(n, _)| n != "repeats.csv");
    files.push((
        "repeats.csv".into(),
        b"container,field,count\n\
          response:readDtcByStatusMask,records,end\n\
          layout:dtcStatus,nested,end\n"
            .to_vec(),
    ));
    // Both DTCs name the layout; the field is reported once.
    assert_eq!(
        resolved(files),
        vec![at(BadValue, "fields.csv", Some(line), Some("shape"))]
    );
}

#[test]
fn every_scoped_key_is_unique_within_a_variant() {
    let rejected = |file: &str, header: &str, rows: &[&str], column: &str| {
        let line = u32::try_from(rows.len()).unwrap() + 1;
        assert_eq!(
            resolved_with(file, header, rows),
            vec![at(Duplicate, file, Some(line), Some(column))],
            "{file}"
        );
    };
    // In each, the first two rows are in different variants; the last clashes with both.
    rejected(
        "protocols.csv",
        "variants,name,baud_rate,tx_id,rx_id",
        &[
            "exampleEcuV1,ISO15765,500000,0x7E0,0x7E8",
            "exampleEcuV2,ISO15765,500000,0x7E0,0x7E8",
            ",ISO15765,250000,0x7E0,0x7E8",
        ],
        "name",
    );
    rejected(
        "comparams.csv",
        "variants,protocol,name,value",
        &[
            "exampleEcuV1,ISO15765,CP_StMin,0",
            "exampleEcuV2,ISO15765,CP_StMin,1",
            ",ISO15765,CP_StMin,2",
        ],
        "name",
    );
    rejected(
        "routines.csv",
        "variants,name,rid,start",
        &[
            "exampleEcuV1,eraseMemory,0xFF00,yes",
            "exampleEcuV2,eraseAgain,0xFF00,yes",
            ",checkProgrammingDependencies,0xFF00,yes",
        ],
        "rid",
    );
    rejected(
        "dtcs.csv",
        "variants,code,display,status",
        &[
            "exampleEcuV1,0x910116,B1101-16,dtcStatus",
            "exampleEcuV2,0x910116,B1101-16,dtcStatus",
            ",0x910116,B1101-16,dtcStatus",
        ],
        "code",
    );
    let mut files = with(
        "flash_sessions.csv",
        "variants,id,name,steps\n\
         exampleEcuV1,1,application,ecu-reset\n\
         exampleEcuV2,1,calibration,ecu-reset\n\
         ,1,boot,ecu-reset\n",
    );
    files.retain(|(n, _)| n != "flash_segments.csv");
    files.push((
        "flash_segments.csv".into(),
        b"session,name,start_address,length,artifact_offset,block_size\n\
          application,app,0x0,0x10,0,0x10\n\
          calibration,cal,0x0,0x10,0,0x10\n\
          boot,boot,0x0,0x10,0,0x10\n"
            .to_vec(),
    ));
    assert_eq!(
        resolved(files),
        vec![at(Duplicate, "flash_sessions.csv", Some(4), Some("id"))]
    );
}

#[test]
fn an_unknown_variant_hides_no_other_error_of_its_row() {
    assert_eq!(
        resolved_with(
            "comparams.csv",
            "variants,protocol,name,value",
            &["exampleEcuV3,CAN,CP_StMin,0"],
        ),
        vec![
            at(UnknownReference, "comparams.csv", Some(2), Some("protocol")),
            at(UnknownReference, "comparams.csv", Some(2), Some("variants")),
        ]
    );
    assert_eq!(
        resolved_with(
            "variant_patterns.csv",
            "variant,service,field,expected",
            &[
                "exampleEcuV3,missing.read,softwareVersion,31",
                "exampleEcuV3,softwareVersion.read,missing,31",
            ],
        ),
        vec![
            at(
                UnknownReference,
                "variant_patterns.csv",
                Some(2),
                Some("service")
            ),
            at(
                UnknownReference,
                "variant_patterns.csv",
                Some(2),
                Some("variant")
            ),
            at(
                UnknownReference,
                "variant_patterns.csv",
                Some(3),
                Some("field")
            ),
            at(
                UnknownReference,
                "variant_patterns.csv",
                Some(3),
                Some("variant")
            ),
        ]
    );
    // A service whose own scope has a typo still has its response fields checked.
    let dids =
        example_text("dids.csv").replace("\n,softwareVersion,", "\nexampleEcuV11,softwareVersion,");
    let mut files = with("dids.csv", &dids);
    files.retain(|(n, _)| n != "variant_patterns.csv");
    files.push((
        "variant_patterns.csv".into(),
        b"variant,service,field,expected\nexampleEcuV1,softwareVersion.read,missing,31\n".to_vec(),
    ));
    assert_eq!(
        resolved(files),
        vec![
            at(UnknownReference, "dids.csv", Some(4), Some("variants")),
            at(
                UnknownReference,
                "variant_patterns.csv",
                Some(2),
                Some("field")
            ),
        ]
    );
}

// ---------------------------------------------------------------- Rules of single fields

/// The columns of the example's `fields.csv` followed by `max_bytes` and `byte_order`.
const FIELD_COLUMNS: &str = "container,name,byte,bit,type,bits,length,min_bytes,terminator,\
                             conversion,unit,const,shape,layout,ref,max_bytes,byte_order";

/// The example with `rows` (in [`FIELD_COLUMNS`] order, trailing cells optional) after its
/// fields, and `services` after its services (`variants,name,sid`); the line of the first added
/// field row.
fn with_fields(rows: &[&str], services: &[&str]) -> (Vec<(String, Vec<u8>)>, u32) {
    let example = example_text("fields.csv");
    let mut lines = example.lines();
    let header = lines.next().expect("header");
    let width = FIELD_COLUMNS.split(',').count();
    let pad = |row: &str| {
        let cells = row.split(',').count();
        assert!(cells <= width, "{row}");
        format!("{row}{}\n", ",".repeat(width - cells))
    };
    let mut fields = format!("{header},max_bytes,byte_order\n");
    for line in lines {
        fields.push_str(&format!("{line},,\n"));
    }
    let first = u32::try_from(fields.lines().count()).unwrap() + 1;
    for row in rows {
        fields.push_str(&pad(row));
    }
    let mut services_csv = String::from("variants,name,sid\n");
    for line in example_text("services.csv").lines().skip(1) {
        services_csv.push_str(&format!(",{line}\n"));
    }
    for row in services {
        services_csv.push_str(row);
        services_csv.push('\n');
    }
    let mut files = with("fields.csv", &fields);
    files.retain(|(n, _)| n != "services.csv");
    files.push(("services.csv".into(), services_csv.into_bytes()));
    (files, first)
}

/// The errors of [`with_fields`] in `fields.csv`, as (code, index of the added row, column);
/// an error of the row as a whole has the column "".
fn field_errors(rows: &[&str], services: &[&str]) -> Vec<(ErrorCode, u32, String)> {
    let (files, first) = with_fields(rows, services);
    resolved(files)
        .into_iter()
        .map(|(code, file, line, column)| {
            assert_eq!(file, "fields.csv", "{code:?} at {line:?}");
            (code, line.unwrap() - first, column.unwrap_or_default())
        })
        .collect()
}

fn row(code: ErrorCode, index: u32, column: &str) -> (ErrorCode, u32, String) {
    (code, index, column.to_owned())
}

#[test]
fn a_field_has_the_width_and_columns_of_its_type_and_length() {
    const L: &str = "layout:watchdogReset";
    let rows = [
        format!("{L},a,5,,uint,65"),
        format!("{L},b,6,3,float,32"),
        format!("{L},c,,,bcd,6"),
        format!("{L},d,,,unicode2,24"),
        format!("{L},e,,,uint"),
        format!("{L},f,,,uint,8,leading"),
        format!("{L},g,,,ascii,40,leading"),
        format!("{L},h,,,ascii,8,terminated"),
        format!("{L},i,,,ascii,,terminated,4,,,,,,,,2"),
        format!("{L},j,,,uint,8,,1"),
        format!("{L},k,,,ascii,,field"),
        format!("{L},l,,,bytes,,field,,,,,,,,g"),
        format!("{L},m,,,uint,8,,,,,,,,,taskId"),
        format!("{L},n,,,ascii,8,,,,supplyVoltage"),
        format!("{L},o,,,float,32,,,,sessionName"),
        format!("{L},p,,2,ascii,8"),
        "did:powerCycles,q,536870911,,uint,8".to_owned(),
        // Valid: a length from a fixed uint, a terminated string within its limits.
        format!("{L},r,,,ascii,,field,,,,,,,,taskId"),
        format!("{L},s,,,utf8,,terminated,2,zero,,,,,,,8"),
    ];
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    assert_eq!(
        field_errors(&rows, &[]),
        vec![
            row(BadValue, 0, "bits"),
            row(BadValue, 1, "bit"),
            row(BadValue, 2, "bits"),
            row(BadValue, 3, "bits"),
            row(MissingColumn, 4, "bits"),
            row(IrRule, 5, "length"),
            row(BadValue, 6, "bits"),
            row(BadValue, 7, "bits"),
            row(IrRule, 8, "min_bytes"),
            row(BadValue, 9, "min_bytes"),
            row(MissingColumn, 10, "ref"),
            row(BadValue, 11, "ref"),
            row(BadValue, 12, "ref"),
            row(BadValue, 13, "conversion"),
            row(IrRule, 14, "conversion"),
            row(BadValue, 15, "bit"),
            row(BadValue, 16, "byte"),
        ]
    );
}

#[test]
fn a_ref_names_neither_a_constant_nor_a_shaped_field() {
    assert_eq!(
        field_errors(
            &[
                "response:defaultSession,n,,,ascii,,field,,,,,,,,sub",
                "response:readDtcByStatusMask,n,,,ascii,,field,,,,,,,,records",
            ],
            &[],
        ),
        vec![row(UnknownReference, 0, "ref"), row(BadValue, 1, "ref")]
    );
}

#[test]
fn constants_lead_their_container_and_fit_their_field() {
    let services: Vec<String> = (1..=7).map(|i| format!(",t{i},0x8{i}")).collect();
    let services: Vec<&str> = services.iter().map(String::as_str).collect();
    assert_eq!(
        field_errors(
            &[
                "layout:watchdogReset,k,,,uint,8,,,,,,0x01",
                "response:extendedSession,late,5,,uint,8,,,,,,0x01",
                "response:t1,k,1,,uint,8,,,,,,0x01",
                "response:t2,k,,,uint,12,,,,,,0x01",
                "response:t3,k,,,uint,8,,,,,,0x100",
                "response:t4,k,,,bytes,16,,,,,,F1",
                "response:t5,k,,,ascii,8,,,,,,31",
                "response:t6,k,,,uint,8,,,,,millisecond,0x01,,,,,little",
                "response:t7,a,,,uint,8,,,,,,0x01",
                "response:t7,b,2,,uint,8,,,,,,0x02",
                // Valid: a 2-byte identifier then a 1-byte sub-function.
                "response:t7,c,3,,bytes,16,,,,,,F1 90",
            ],
            &services,
        ),
        vec![
            row(Unsupported, 0, "const"),
            row(Unsupported, 1, "const"),
            row(BadValue, 2, "byte"),
            row(BadValue, 3, "bits"),
            row(BadValue, 4, "const"),
            row(BadValue, 5, "const"),
            row(BadValue, 6, "type"),
            row(BadValue, 7, "byte_order"),
            row(BadValue, 7, "unit"),
            row(BadValue, 9, "byte"),
        ]
    );
}

#[test]
fn a_request_value_has_one_encoding_and_follows_the_previous_field() {
    let services: Vec<String> = (1..=8).map(|i| format!(",r{i},0x9{i}")).collect();
    let services: Vec<&str> = services.iter().map(String::as_str).collect();
    assert_eq!(
        field_errors(
            &[
                "request:r1,n,,,uint,8",
                "request:r1,a,,,ascii,,field,,,,,,,,n",
                "request:r2,a,,,ascii,,terminated,,zero",
                "request:r3,a,,,ascii,,terminated",
                "request:r3,b,,,uint,8",
                "request:r4,a,,,uint,4",
                "request:r5,a,,2,uint,8",
                "request:r6,a,,,uint,8",
                "request:r6,b,2,,uint,8",
                "request:r7,a,,,ascii,8,leading",
                "request:r7,b,3,,uint,8",
                // Valid: one field after another, then a value up to the end.
                "request:r8,a,0,,uint,8",
                "request:r8,b,1,,uint,16",
                "request:r8,c,,,ascii,,terminated",
            ],
            &services,
        ),
        vec![
            row(Unsupported, 1, "length"),
            row(Unsupported, 2, "terminator"),
            row(Unsupported, 3, "length"),
            row(Unsupported, 5, "bits"),
            row(Unsupported, 6, "bit"),
            row(Unsupported, 8, "byte"),
            row(Unsupported, 10, "byte"),
        ]
    );
}

#[test]
fn a_status_layout_lies_within_byte_0() {
    assert_eq!(
        field_errors(
            &[
                "layout:dtcStatus,extra,1,,uint,1",
                "layout:dtcStatus,wide,0,4,uint,8",
                "layout:dtcStatus,after,,,uint,1",
            ],
            &[],
        ),
        vec![
            row(BadValue, 0, "byte"),
            row(BadValue, 1, "bits"),
            row(BadValue, 2, "byte"),
        ]
    );
}

#[test]
fn services_one_call_cannot_tell_apart_are_rejected() {
    let (files, first_field) = with_fields(
        &[
            "request:dup3,sub,,,uint,8,,,,,,0x04",
            "request:dup1,sub,,,uint,8,,,,,,0x01",
            "request:dup2,sub,,,uint,8",
            "request:readVin,did,,,uint,16,,,,,,0xF190",
            "request:readOther,did,,,uint,16,,,,,,0x1234",
        ],
        &[
            ",dup3,0x10",
            ",dup1,0x10",
            ",dup2,0x10",
            ",readVin,0x22",
            ",readOther,0x22",
            "exampleEcuV1,v1only,0x30",
            "exampleEcuV2,v2only,0x30",
        ],
    );
    assert!(first_field > 1);
    // services.csv: the header, the example's four services, then the added ones from line 6.
    assert_eq!(
        resolved(files),
        vec![
            at(Duplicate, "services.csv", Some(7), Some("name")),
            at(Duplicate, "services.csv", Some(8), Some("name")),
            at(Duplicate, "services.csv", Some(9), Some("name")),
        ]
    );
}

#[test]
fn field_rules_report_each_cell_once_and_follow_the_ir() {
    const L: &str = "layout:watchdogReset";
    let rows = [
        // A width that would overflow the status-layout sum is a bad width, no panic.
        "layout:dtcStatus,x,0,1,uint,4294967295".to_owned(),
        // A leading length prefix may start at any bit.
        format!("{L},s,,3,ascii,8,leading"),
        format!("{L},u,,,unicode2,,terminated,3,,,,,,,,5"),
        format!("{L},v,,,uint,8,terminated"),
        format!("{L},w,,,uint,8,,,,,,,,,,4"),
        format!("{L},w2,,,uint,8,,,end"),
        format!("{L},y,,,ascii,,leading"),
    ];
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    assert_eq!(
        field_errors(&rows, &[]),
        vec![
            row(BadValue, 0, "bits"),
            row(IrRule, 2, "max_bytes"),
            row(IrRule, 2, "min_bytes"),
            row(BadValue, 3, "bits"),
            row(IrRule, 3, "length"),
            row(BadValue, 4, "max_bytes"),
            row(BadValue, 5, "terminator"),
            row(MissingColumn, 6, "bits"),
        ]
    );
}

#[test]
fn every_container_that_is_sent_is_checked_once_per_cause() {
    assert_eq!(
        field_errors(
            &[
                // A missing width is reported; the next field's position is not.
                "request:d1,a,0,,uint",
                "request:d1,v,1,,uint,8",
                "request:d2,sub,0,,uint,8,,,,,,0x01",
                "request:d2,v,2,,uint,8",
                // After a value of variable length, no field has a known position.
                "request:d3,a,,,ascii,8,leading",
                "request:d3,b,,,uint,8",
                "request:d3,c,5,,uint,8",
                // A routine request and the record of a DID with a write service are sent.
                "routine:eraseMemory:start:request,a,,4,uint,8",
                "did:vin,extra,20,,uint,8",
                // The record of a DID without one is not.
                "did:powerCycles,gap,10,,uint,8",
            ],
            &[",d1,0xA1", ",d2,0xA2", ",d3,0xA3"],
        ),
        vec![
            row(MissingColumn, 0, "bits"),
            row(Unsupported, 3, "byte"),
            row(Unsupported, 6, "byte"),
            row(Unsupported, 7, "bit"),
            row(Unsupported, 8, "byte"),
        ]
    );
}

#[test]
fn constants_are_compared_by_value_across_services() {
    let rows = [
        "request:a16,sub,,,uint,16,,,,,,0x0001",
        "request:b8,sub,,,uint,8,,,,,,0x101",
        "request:bytesSub,sub,,,bytes,8,,,,,,01",
        "request:writeVin,did,,,uint,16,,,,,,0xF190",
        "request:writeVin,vin,,,ascii,136",
        "request:routineLike,sub,,,uint,8,,,,,,0x01",
        "request:routineLike,rid,,,uint,16,,,,,,0xFF00",
    ];
    let (files, first) = with_fields(
        &rows,
        &[
            ",a16,0x10",
            ",b8,0x10",
            ",bytesSub,0x10",
            ",writeVin,0x2E",
            ",routineLike,0x31",
        ],
    );
    assert_eq!(
        resolved(files),
        vec![
            // A broken constant is reported once, with no clash made up from it.
            at(BadValue, "fields.csv", Some(first + 1), Some("const")),
            // 0x0001 in 16 bits is the value 1, as is defaultSession's 0x01.
            at(Duplicate, "services.csv", Some(6), Some("name")),
            // The same request fields as the generated vin.write.
            at(Duplicate, "services.csv", Some(9), Some("name")),
        ]
    );
}

#[test]
fn a_counted_repeat_counts_with_a_fixed_number() {
    let (mut files, first) = with_fields(
        &[
            "response:rr,cnt,,,ascii,8",
            "response:rr,items,,,,,,,,,,,repeat,dtcRecord,cnt",
        ],
        &[",rr,0xB1"],
    );
    files.retain(|(n, _)| n != "repeats.csv");
    files.push((
        "repeats.csv".into(),
        b"container,field,count\n\
          response:readDtcByStatusMask,records,end\n\
          response:rr,items,field\n"
            .to_vec(),
    ));
    assert_eq!(
        resolved(files),
        vec![at(BadValue, "fields.csv", Some(first + 1), Some("ref"))]
    );
}

#[test]
fn an_error_neither_hides_another_nor_makes_one_up() {
    assert_eq!(
        field_errors(
            &[
                // A bad width gives no known end, so the next position is not judged.
                "request:e1,a,0,,uint,72",
                "request:e1,b,1,,uint,8",
                "request:e2,sub,0,,uint,72,,,,,,0x01",
                "request:e2,v,1,,uint,8",
                // A length source with its own error is not judged again through the ref.
                "layout:watchdogReset,src,,,uint,8,leading",
                "layout:watchdogReset,ln,,,ascii,,field,,,,,,,,src",
                // Every column of a constant is checked, wherever it stands.
                "response:t8,k,,,uint,8,,,,,millisecond,0x01,group,dtcRecord,,,little",
                "layout:watchdogReset,kc,,,uint,8,,,,,millisecond,0x01",
                // A constant's syntax is checked whatever its width.
                "response:t9,k,,,uint,12,,,,,,bogus",
                "response:t10,k,,,bytes,12,,,,,,ZZ",
                // A float's alignment is checked without a width.
                "layout:watchdogReset,fl,,3,float",
            ],
            &[",e1,0x41", ",e2,0x42", ",t8,0x48", ",t9,0x49", ",t10,0x4A"],
        ),
        vec![
            row(BadValue, 0, "bits"),
            row(BadValue, 2, "bits"),
            row(IrRule, 4, "length"),
            // A shaped field has neither a type nor a width (5.4).
            row(BadValue, 6, "bits"),
            row(BadValue, 6, "byte_order"),
            row(BadValue, 6, "shape"),
            row(BadValue, 6, "type"),
            row(BadValue, 6, "unit"),
            row(Unsupported, 7, "const"),
            row(BadValue, 7, "unit"),
            row(BadValue, 8, "bits"),
            row(BadValue, 8, "const"),
            row(BadValue, 9, "bits"),
            row(BadValue, 9, "const"),
            row(BadValue, 10, "bit"),
            row(MissingColumn, 10, "bits"),
        ]
    );
}

#[test]
fn a_rejected_position_starts_nothing_and_a_status_byte_may_be_terminated() {
    assert_eq!(
        field_errors(
            &[
                "response:t11,k,,,uint,72,,,,,,0x01,group,dtcRecord",
                // Only the second field is out of place; the third follows it as written.
                "request:r9,a,0,,uint,8",
                "request:r9,b,2,,uint,8",
                "request:r9,c,2,,uint,8",
                "request:r10,sub,1,,uint,8,,,,,,0x01",
                "request:r10,v,1,,uint,8",
                // At most one byte, so within byte 0; a leading length is not.
                "layout:dtcStatus,tb,0,,bytes,,terminated,1,end,,,,,,,1",
                "layout:dtcStatus,lb,0,,bytes,8,leading",
            ],
            &[",t11,0x4B", ",r9,0x59", ",r10,0x5A"],
        ),
        vec![
            row(BadValue, 0, "bits"),
            row(BadValue, 0, "shape"),
            // A shaped field has no type (5.4).
            row(BadValue, 0, "type"),
            row(Unsupported, 2, "byte"),
            row(BadValue, 4, "byte"),
            row(BadValue, 7, "length"),
        ]
    );
}

#[test]
fn every_position_after_a_variable_value_is_reported() {
    assert_eq!(
        field_errors(
            &[
                "request:r11,a,,,ascii,8,leading",
                "request:r11,b,5,,uint,8",
                "request:r11,c,6,,uint,8",
                // A rejected length kind gives no extent to judge the next field by.
                "request:r12,a,0,,uint,8,leading",
                "request:r12,b,1,,uint,8",
            ],
            &[",r11,0x5B", ",r12,0x5C"],
        ),
        vec![
            row(Unsupported, 1, "byte"),
            row(Unsupported, 2, "byte"),
            row(IrRule, 3, "length"),
        ]
    );
}

// ---------------------------------------------------------------- Nested layouts

/// [`with_fields`], with `extra` rows appended to other tables of the example.
fn with_layouts(
    rows: &[&str],
    services: &[&str],
    extra: &[(&str, &[&str])],
) -> (Vec<(String, Vec<u8>)>, u32) {
    let (mut files, first) = with_fields(rows, services);
    for (file, lines) in extra {
        let entry = files
            .iter_mut()
            .find(|(n, _)| n == file)
            .expect("an example table");
        for line in *lines {
            entry.1.extend_from_slice(line.as_bytes());
            entry.1.push(b'\n');
        }
    }
    (files, first)
}

/// The line of the `index`th row of an example table after its header and existing rows.
fn added_line(file: &str, index: u32) -> u32 {
    u32::try_from(example_text(file).lines().count()).unwrap() + 1 + index
}

#[test]
fn a_shaped_field_has_only_the_columns_of_its_shape() {
    const L: &str = "layout:watchdogReset";
    let rows = [
        format!("{L},g1,2,,uint,8,,,,,,,group,dtcRecord"),
        format!("{L},g2,3,4,,,,,,,,,group,dtcRecord"),
        format!("{L},g3,4,,,,,,,,,,group"),
        format!("{L},g4,5,,,,,,,,,,group,dtcRecord,taskId"),
        "request:s1,g,0,,,,,,,,,,group,dtcRecord".to_owned(),
        format!("{L},r1,6,,,,,,,,,,repeat,dtcRecord"),
        format!("{L},s1,7,,,,,,,,,,select,dtcRecord,taskId"),
        format!("{L},s2,8,,,,,,,,,,select"),
        format!("{L},fk,9,,float,32"),
        format!("{L},s3,13,,,,,,,,,,select,,fk"),
        format!("{L},bk,13,,bytes,72"),
        format!("{L},s4,22,,,,,,,,,,select,,bk"),
    ];
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    assert_eq!(
        field_errors(&rows, &[",s1,0x5D"]),
        vec![
            row(BadValue, 0, "bits"),
            row(BadValue, 0, "type"),
            row(BadValue, 1, "bit"),
            row(MissingColumn, 2, "layout"),
            row(BadValue, 3, "ref"),
            row(Unsupported, 4, "shape"),
            // A missing repeats.csv row is an error of the row, not of a cell.
            row(MissingColumn, 5, ""),
            row(BadValue, 6, "layout"),
            row(MissingColumn, 7, "ref"),
            row(IrRule, 9, "ref"),
            row(IrRule, 11, "ref"),
        ]
    );
}

#[test]
fn repeats_and_cases_belong_to_their_shape_and_cases_fit_their_key() {
    const L: &str = "layout:watchdogReset";
    let rows = [
        format!("{L},k64,30,,uint,64"),
        format!("{L},sel64,38,,,,,,,,,,select,,k64"),
        format!("{L},ki,46,,int,8"),
        format!("{L},seli,47,,,,,,,,,,select,,ki"),
    ];
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let (files, _) = with_layouts(
        &rows,
        &[],
        &[
            ("repeats.csv", &["layout:watchdogReset,taskId,end"]),
            (
                "select_cases.csv",
                &[
                    "did:lastResetReason,details,overlap,0x05,0x06,",
                    "did:lastResetReason,details,big,0x100,,",
                    "did:lastResetReason,details,neg,-1,,",
                    "did:lastResetReason,details,dflt,,,",
                    "did:lastResetReason,details,dflt2,,,",
                    "did:lastResetReason,reason,notSel,0x01,,",
                    "layout:watchdogReset,sel64,cross,0x7FFFFFFFFFFFFFFF,0x8000000000000000,",
                    "layout:watchdogReset,sel64,top,0xFFFFFFFFFFFFFFFF,,",
                    "layout:watchdogReset,seli,hi,0x80,,",
                    "layout:watchdogReset,seli,lo,-128,-1,",
                ],
            ),
        ],
    );
    const S: &str = "select_cases.csv";
    let s = |i| Some(added_line(S, i));
    assert_eq!(
        resolved(files),
        vec![
            at(
                BadValue,
                "repeats.csv",
                Some(added_line("repeats.csv", 0)),
                Some("field")
            ),
            at(Duplicate, S, s(0), Some("lower")),
            at(BadValue, S, s(1), Some("lower")),
            at(BadValue, S, s(2), Some("lower")),
            at(Duplicate, S, s(4), Some("case")),
            at(BadValue, S, s(5), Some("field")),
            at(BadValue, S, s(6), Some("upper")),
            at(BadValue, S, s(8), Some("lower")),
        ]
    );
}

#[test]
fn layouts_hold_their_fields_reach_no_cycle_and_nest_at_most_16_deep() {
    let mut rows = vec![
        "layout:dtcRecord,over,3,,uint,16".to_owned(),
        "layout:loopA,b,0,,,,,,,,,,group,loopB".to_owned(),
        "layout:loopB,a,0,,,,,,,,,,group,loopA".to_owned(),
        "layout:selfL,me,0,,,,,,,,,,group,selfL".to_owned(),
        // A response reaching 17 layouts, and one reaching 16.
        "response:deep,top,1,,,,,,,,,,group,d1".to_owned(),
        "response:shallow,top,1,,,,,,,,,,group,d2".to_owned(),
    ];
    let mut layouts = vec![
        "loopA,0".to_owned(),
        "loopB,0".to_owned(),
        "selfL,0".to_owned(),
    ];
    for i in 1..=17 {
        layouts.push(format!("d{i},0"));
        rows.push(if i < 17 {
            format!("layout:d{i},n,0,,,,,,,,,,group,d{}", i + 1)
        } else {
            format!("layout:d{i},v,0,,uint,8")
        });
    }
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let layouts: Vec<&str> = layouts.iter().map(String::as_str).collect();
    let (files, first) = with_layouts(
        &rows,
        &[",deep,0x5E", ",shallow,0x5F"],
        &[("layouts.csv", &layouts)],
    );
    assert_eq!(
        resolved(files),
        vec![
            at(IrRule, "fields.csv", Some(first), Some("byte")),
            at(IrRule, "fields.csv", Some(first + 4), Some("shape")),
            at(
                IrRule,
                "layouts.csv",
                Some(added_line("layouts.csv", 0)),
                Some("name")
            ),
            at(
                IrRule,
                "layouts.csv",
                Some(added_line("layouts.csv", 2)),
                Some("name")
            ),
        ]
    );
}

#[test]
fn a_select_key_with_an_error_of_its_own_judges_no_case() {
    const L: &str = "layout:watchdogReset";
    let rows = [
        format!("{L},fk,9,,float,32"),
        format!("{L},s3,13,,,,,,,,,,select,,fk"),
        format!("{L},sk,14,,,,,,,,,,group,dtcRecord"),
        format!("{L},s5,15,,,,,,,,,,select,,sk"),
        format!("{L},bk2,20,,bytes,12"),
        format!("{L},s6,22,,,,,,,,,,select,,bk2"),
        // Bytes keys are compared for equality, and at most 8 bytes long.
        format!("{L},bk3,24,,bytes,16"),
        format!("{L},s7,26,,,,,,,,,,select,,bk3"),
        format!("{L},tk,28,,ascii,,terminated"),
        format!("{L},s8,40,,,,,,,,,,select,,tk"),
        format!("{L},tk2,50,,ascii,,terminated,1,,,,,,,,4"),
        format!("{L},s9,60,,,,,,,,,,select,,tk2"),
        // A BCD key holds decimal values.
        format!("{L},bcdk,70,,bcd,8"),
        format!("{L},s10,71,,,,,,,,,,select,,bcdk"),
        format!("{L},ul,72,,uint,8,,,,,,,,dtcRecord"),
        "did:vin,g,17,,,,,,,,,,group,dtcRecord".to_owned(),
    ];
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let (files, first) = with_layouts(
        &rows,
        &[],
        &[(
            "select_cases.csv",
            &[
                "layout:watchdogReset,s3,neg,-1,,",
                "layout:watchdogReset,s5,neg,-1,,",
                "layout:watchdogReset,s6,c,0x1000,,",
                "layout:watchdogReset,s7,r,0x03,0x0F,",
                "layout:watchdogReset,s7,one,0x10,,",
                "layout:watchdogReset,s9,one,0x41,,",
                "layout:watchdogReset,s10,ok,99,,",
                "layout:watchdogReset,s10,hi,100,,",
            ],
        )],
    );
    const S: &str = "select_cases.csv";
    let f = |i: u32, code, column| at(code, "fields.csv", Some(first + i), Some(column));
    assert_eq!(
        resolved(files),
        vec![
            f(1, IrRule, "ref"),
            f(3, BadValue, "ref"),
            f(4, BadValue, "bits"),
            f(9, IrRule, "ref"),
            f(14, BadValue, "layout"),
            f(15, Unsupported, "shape"),
            at(BadValue, S, Some(added_line(S, 3)), Some("upper")),
            at(BadValue, S, Some(added_line(S, 7)), Some("lower")),
        ]
    );
}

#[test]
fn cycles_and_depth_follow_every_shape_and_items_hold_their_layout() {
    let mut rows = vec![
        "layout:cs,k,0,,uint,8".to_owned(),
        "layout:cs,sw,1,,,,,,,,,,select,,k".to_owned(),
        "layout:cr,items,0,,,,,,,,,,repeat,cr".to_owned(),
        // A cycle below hides no chain that is too deep.
        "layout:top,c,0,,,,,,,,,,group,cs".to_owned(),
        "layout:top,d,1,,,,,,,,,,group,e1".to_owned(),
        "response:deep2,f,1,,,,,,,,,,group,top".to_owned(),
        // A select case leads to its layout as a group does.
        "response:viaSel,k,1,,uint,8".to_owned(),
        "response:viaSel,s,2,,,,,,,,,,select,,k".to_owned(),
        // dtcRecord's fields reach byte 4.
        "response:fx,items,1,,,,,,,,,,repeat,dtcRecord".to_owned(),
        "response:fx,ok,9,,,,,,,,,,repeat,dtcRecord".to_owned(),
        "layout:dtcRecord,late,5,,,,,,,,,,group,watchdogReset".to_owned(),
    ];
    let mut layouts = vec!["cs,0".to_owned(), "cr,0".to_owned(), "top,0".to_owned()];
    for i in 1..=17 {
        layouts.push(format!("e{i},0"));
        rows.push(if i < 17 {
            format!("layout:e{i},n,0,,,,,,,,,,group,e{}", i + 1)
        } else {
            format!("layout:e{i},v,0,,uint,8")
        });
    }
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let layouts: Vec<&str> = layouts.iter().map(String::as_str).collect();
    let (mut files, first) = with_layouts(
        &rows,
        &[",deep2,0x60", ",viaSel,0x61", ",fx,0x62"],
        &[
            ("layouts.csv", &layouts),
            (
                "select_cases.csv",
                &["layout:cs,sw,me,0x01,,cs", "response:viaSel,s,x,0x01,,e1"],
            ),
        ],
    );
    files.retain(|(n, _)| n != "repeats.csv");
    files.push((
        "repeats.csv".into(),
        b"container,field,count,items,item_bytes\n\
          response:readDtcByStatusMask,records,end,,\n\
          layout:cr,items,end,,\n\
          response:fx,items,fixed,2,2\n\
          response:fx,ok,fixed,1,4\n"
            .to_vec(),
    ));
    let f = |i: u32, column| at(IrRule, "fields.csv", Some(first + i), Some(column));
    assert_eq!(
        resolved(files),
        vec![
            f(5, "shape"),
            f(7, "shape"),
            f(10, "byte"),
            at(
                IrRule,
                "layouts.csv",
                Some(added_line("layouts.csv", 0)),
                Some("name")
            ),
            at(
                IrRule,
                "layouts.csv",
                Some(added_line("layouts.csv", 1)),
                Some("name")
            ),
            at(
                IrRule,
                "repeats.csv",
                Some(added_line("repeats.csv", 1)),
                Some("item_bytes")
            ),
        ]
    );
}

#[test]
fn a_long_chain_of_layouts_is_checked_without_recursion() {
    let mut rows = vec!["response:chain,f,1,,,,,,,,,,group,c1".to_owned()];
    let mut layouts = Vec::new();
    const N: usize = 5000;
    for i in 1..=N {
        layouts.push(format!("c{i},0"));
        rows.push(if i < N {
            format!("layout:c{i},n,0,,,,,,,,,,group,c{}", i + 1)
        } else {
            format!("layout:c{i},v,0,,uint,8")
        });
    }
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let layouts: Vec<&str> = layouts.iter().map(String::as_str).collect();
    let (files, first) = with_layouts(&rows, &[",chain,0x63"], &[("layouts.csv", &layouts)]);
    assert_eq!(
        resolved(files),
        vec![at(IrRule, "fields.csv", Some(first), Some("shape"))]
    );
}

#[test]
fn a_terminated_key_and_a_shaped_item_field_have_their_bounds() {
    const L: &str = "layout:watchdogReset";
    let rows = [
        format!("{L},tk1,50,,bytes,,terminated,1,,,,,,,,1"),
        format!("{L},s11,60,,,,,,,,,,select,,tk1"),
        // An item of one byte whose layout places a group at byte 2.
        "response:fy,items,1,,,,,,,,,,repeat,itemL".to_owned(),
        "layout:itemL,g,2,,,,,,,,,,group,dtcStatus".to_owned(),
    ];
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let (mut files, _) = with_layouts(
        &rows,
        &[",fy,0x64"],
        &[
            ("layouts.csv", &["itemL,0"]),
            (
                "select_cases.csv",
                &[
                    "layout:watchdogReset,s11,one,0xFF,,",
                    "layout:watchdogReset,s11,two,0x100,,",
                ],
            ),
        ],
    );
    files.retain(|(n, _)| n != "repeats.csv");
    files.push((
        "repeats.csv".into(),
        b"container,field,count,items,item_bytes\n\
          response:readDtcByStatusMask,records,end,,\n\
          response:fy,items,fixed,3,1\n"
            .to_vec(),
    ));
    const S: &str = "select_cases.csv";
    assert_eq!(
        resolved(files),
        vec![
            at(IrRule, "repeats.csv", Some(3), Some("item_bytes")),
            at(BadValue, S, Some(added_line(S, 1)), Some("lower")),
        ]
    );
}

#[test]
fn many_selects_cycles_and_sized_layouts_are_checked_in_linear_time() {
    const N: usize = 3000;
    let mut rows = Vec::new();
    let mut layouts = Vec::new();
    let mut cases = Vec::new();
    for i in 0..N {
        // A select per layout leading to the next, a self cycle, and a sized layout.
        layouts.push(format!("m{i},2"));
        rows.push(format!("layout:m{i},k,0,,uint,8"));
        rows.push(format!("layout:m{i},s,1,,,,,,,,,,select,,k"));
        if i + 1 < N {
            cases.push(format!("layout:m{i},s,next,0x01,,m{}", i + 1));
        }
        layouts.push(format!("self{i},0"));
        rows.push(format!("layout:self{i},me,0,,,,,,,,,,group,self{i}"));
    }
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let layouts: Vec<&str> = layouts.iter().map(String::as_str).collect();
    let cases: Vec<&str> = cases.iter().map(String::as_str).collect();
    let (files, _) = with_layouts(
        &rows,
        &[],
        &[("layouts.csv", &layouts), ("select_cases.csv", &cases)],
    );
    let started = std::time::Instant::now();
    let errors = resolved(files);
    // One cycle error per self-referring layout, and nothing else.
    assert_eq!(errors.len(), N);
    assert!(errors.iter().all(|e| e.0 == IrRule && e.1 == "layouts.csv"));
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn a_key_missing_its_width_and_large_selects_and_shared_items_are_checked_once() {
    const L: &str = "layout:watchdogReset";
    const CASES: u32 = 20_000;
    const REPEATS: usize = 2_000;
    let mut rows = vec![
        // A bytes key without a width: its own error, nothing on the select.
        format!("{L},nk,40,,bytes"),
        format!("{L},sn,48,,,,,,,,,,select,,nk"),
        // A 16-bit key with many disjoint cases, one overlapping at the end.
        format!("{L},wk,50,,uint,16"),
        format!("{L},sw,52,,,,,,,,,,select,,wk"),
    ];
    // Many fixed repeats sharing one large layout whose last field ends at byte 2,000.
    let mut layouts = vec!["shared,0".to_owned()];
    for i in 0..REPEATS {
        rows.push(format!("layout:shared,f{i},{i},,uint,8"));
    }
    let mut repeats = String::from(
        "container,field,count,items,item_bytes\nresponse:readDtcByStatusMask,records,end,,\n",
    );
    for i in 0..REPEATS {
        rows.push(format!("response:many,r{i},1,,,,,,,,,,repeat,shared"));
        let item_bytes = if i == 0 { 1_999 } else { 2_000 };
        repeats.push_str(&format!("response:many,r{i},fixed,1,{item_bytes}\n"));
    }
    layouts.push("unused,0".to_owned());
    let mut cases: Vec<String> = (0..CASES)
        .map(|i| format!("{L},sw,c{i},{},,", i * 3))
        .collect();
    cases.push(format!("{L},sw,clash,{},{},", 3, 4));
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let layouts: Vec<&str> = layouts.iter().map(String::as_str).collect();
    let cases: Vec<&str> = cases.iter().map(String::as_str).collect();
    let (mut files, first) = with_layouts(
        &rows,
        &[",many,0x65"],
        &[("layouts.csv", &layouts), ("select_cases.csv", &cases)],
    );
    files.retain(|(n, _)| n != "repeats.csv");
    files.push(("repeats.csv".into(), repeats.into_bytes()));
    let started = std::time::Instant::now();
    const S: &str = "select_cases.csv";
    assert_eq!(
        resolved(files),
        vec![
            at(MissingColumn, "fields.csv", Some(first), Some("bits")),
            at(IrRule, "repeats.csv", Some(3), Some("item_bytes")),
            at(Duplicate, S, Some(added_line(S, CASES)), Some("lower")),
        ]
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn a_case_overlapping_only_a_rejected_case_is_reported_too() {
    const L: &str = "layout:watchdogReset";
    let (files, _) = with_layouts(
        &[
            &format!("{L},k8,40,,uint,8"),
            &format!("{L},s8,41,,,,,,,,,,select,,k8"),
        ],
        &[],
        &[(
            "select_cases.csv",
            &[
                "layout:watchdogReset,s8,a,0,10,",
                "layout:watchdogReset,s8,b,5,20,",
                "layout:watchdogReset,s8,c,15,16,",
                "layout:watchdogReset,s8,d,21,30,",
            ],
        )],
    );
    const S: &str = "select_cases.csv";
    assert_eq!(
        resolved(files),
        vec![
            at(Duplicate, S, Some(added_line(S, 1)), Some("lower")),
            at(Duplicate, S, Some(added_line(S, 2)), Some("lower")),
        ]
    );
}

#[test]
fn many_selects_find_their_keys_in_linear_time() {
    const N: usize = 5_000;
    let mut rows = Vec::new();
    let mut layouts = Vec::new();
    let mut cases = Vec::new();
    for i in 0..N {
        layouts.push(format!("q{i},0"));
        rows.push(format!("layout:q{i},k,0,,uint,8"));
        rows.push(format!("layout:q{i},s,1,,,,,,,,,,select,,k"));
        cases.push(format!("layout:q{i},s,one,0x01,,"));
    }
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    let layouts: Vec<&str> = layouts.iter().map(String::as_str).collect();
    let cases: Vec<&str> = cases.iter().map(String::as_str).collect();
    let (files, _) = with_layouts(
        &rows,
        &[],
        &[("layouts.csv", &layouts), ("select_cases.csv", &cases)],
    );
    let started = std::time::Instant::now();
    assert_eq!(resolved(files), vec![]);
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}
