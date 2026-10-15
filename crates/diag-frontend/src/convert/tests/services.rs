use diag_ir::declaration::{
    ByteOrder, CompuT, DataKind, FieldT, LayoutT, LengthKind, NegativeResponseT, Placement,
    RepeatCount, ServiceT, ShapeT, Termination,
};

use super::*;

fn service<'d>(document: &'d EcuDocumentT, name: &str) -> &'d ServiceT {
    variant(document)
        .services
        .iter()
        .flatten()
        .find(|s| s.short_name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no service {name}"))
}

fn request(service: &ServiceT) -> (u8, &[u8], &[FieldT]) {
    let r = service.request.as_deref().expect("a request");
    (
        r.sid,
        r.prefix.as_deref().expect("a prefix"),
        r.params.as_deref().expect("params"),
    )
}

fn response(service: &ServiceT) -> (u8, &[u8], &[FieldT]) {
    let r = service.positive.as_deref().expect("a positive response");
    (
        r.sid,
        r.prefix.as_deref().expect("a prefix"),
        r.fields.as_deref().expect("fields"),
    )
}

fn field<'f>(fields: &'f [FieldT], name: &str) -> &'f FieldT {
    fields
        .iter()
        .find(|f| f.short_name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no field {name}"))
}

fn layouts(document: &EcuDocumentT) -> &[LayoutT] {
    variant(document).layouts.as_deref().expect("layouts")
}

/// A fixed, big-endian field at an absolute bit offset, with no conversion or unit.
fn scalar(id: u32, name: &str, bit_offset: u32, bit_length: u32, kind: DataKind) -> FieldT {
    let mut f = FieldT::default();
    f.id = id;
    f.short_name = Some(name.to_owned());
    f.placement = Placement::Absolute;
    f.bit_offset = bit_offset;
    f.length_kind = LengthKind::Fixed;
    f.bit_length = bit_length;
    f.kind = kind;
    f.compu = CompuT::CompuIdentity(Box::default());
    f
}

fn negatives(service: &ServiceT) -> Vec<(u8, u32)> {
    service
        .negatives
        .iter()
        .flatten()
        .map(|n: &NegativeResponseT| (n.nrc, n.message_id))
        .collect()
}

#[test]
fn the_example_services_have_ids_in_row_order() {
    let documents = documents(&convert(example()));
    let ids = |document: &EcuDocumentT| -> Vec<(u32, String)> {
        variant(document)
            .services
            .iter()
            .flatten()
            .map(|s| (s.id, s.short_name.clone().expect("a name")))
            .collect()
    };
    let mut expected: Vec<(u32, String)> = [
        (1, "defaultSession"),
        (2, "extendedSession"),
        (3, "programmingSession"),
        (4, "readDtcByStatusMask"),
        (5, "activeSession.read"),
        (6, "sparePartNumber.read"),
        (7, "softwareVersion.read"),
        (8, "vin.read"),
        (9, "vin.write"),
        (10, "flashState.read"),
        (11, "powerCycles.read"),
        (12, "supplyVoltage.read"),
        (13, "lastResetReason.read"),
        (14, "eraseMemory.start"),
        (15, "checkProgrammingDependencies.start"),
    ]
    .into_iter()
    .map(|(id, name)| (id, name.to_owned()))
    .collect();
    assert_eq!(ids(&documents[1]), expected);
    // The supply voltage DID is only in the second variant; the ids stay the same.
    expected.retain(|(id, _)| *id != 12);
    assert_eq!(ids(&documents[0]), expected);
}

#[test]
fn a_declared_service_carries_its_constants_as_prefixes() {
    let documents = documents(&convert(example()));
    let session = service(&documents[0], "defaultSession");
    assert_eq!(request(session), (0x10, &[0x01][..], &[][..]));
    let mut p2 = scalar(1, "p2ServerMax", 16, 16, DataKind::Uint);
    p2.unit_id = 2;
    let mut p2_star = scalar(2, "p2StarServerMax", 32, 16, DataKind::Uint);
    p2_star.unit_id = 2;
    let CompuT::CompuLinear(linear) = &response(session).2[1].compu else {
        panic!("a linear conversion")
    };
    assert_eq!(
        (linear.numerator, linear.denominator, linear.offset),
        (10.0, 1.0, 0.0)
    );
    p2_star.compu = response(session).2[1].compu.clone();
    assert_eq!(response(session), (0x50, &[0x01][..], &[p2, p2_star][..]));
    assert_eq!(
        (
            session.required_session,
            session.required_security_level,
            session.p2_millis,
            session.p2_star_millis
        ),
        (0, 0, 50, 5000)
    );
    // The variant-wide NRCs, in row order, with their text ids.
    assert_eq!(
        negatives(session),
        [(0x12, 10), (0x22, 11), (0x31, 12), (0x33, 13)]
    );
    // A request field that is not a constant is a parameter.
    let dtc = service(&documents[0], "readDtcByStatusMask");
    let (sid, prefix, params) = request(dtc);
    assert_eq!((sid, prefix), (0x19, &[0x02][..]));
    assert_eq!(params, [scalar(7, "statusMask", 16, 8, DataKind::Uint)]);
}

#[test]
fn did_services_share_the_record_fields() {
    let documents = documents(&convert(example()));
    // Ids 1 to 22 are the fields of the rows above it in fields.csv that are not constants.
    let vin = scalar(23, "vin", 24, 136, DataKind::Ascii);
    let read = service(&documents[0], "vin.read");
    assert_eq!(request(read), (0x22, &[0xF1, 0x90][..], &[][..]));
    assert_eq!(
        response(read),
        (0x62, &[0xF1, 0x90][..], &[vin.clone()][..])
    );
    let write = service(&documents[0], "vin.write");
    assert_eq!(request(write), (0x2E, &[0xF1, 0x90][..], &[vin][..]));
    assert_eq!(response(write), (0x6E, &[0xF1, 0x90][..], &[][..]));
    assert_eq!(
        (write.required_session, write.required_security_level),
        (0x03, 0x01)
    );
    // A terminated record.
    let spare = &response(service(&documents[0], "sparePartNumber.read")).2[0];
    assert_eq!(
        (
            spare.length_kind,
            spare.bit_length,
            spare.min_bytes,
            spare.max_bytes,
            spare.termination
        ),
        (LengthKind::Terminated, 0, 1, 0, Termination::EndOfPdu)
    );
    // A text table.
    let session = &response(service(&documents[0], "activeSession.read")).2[0];
    let CompuT::CompuTextTable(table) = &session.compu else {
        panic!("a text table")
    };
    let entries: Vec<(i64, i64, u32)> = table
        .entries
        .iter()
        .flatten()
        .map(|e| (e.lower, e.upper, e.text_id))
        .collect();
    assert_eq!(entries, [(1, 1, 1), (2, 2, 2), (3, 3, 3)]);
    assert_eq!(table.default_text_id, 6);
}

#[test]
fn routine_services_carry_the_generated_constants() {
    let documents = documents(&convert(example()));
    let check = service(&documents[0], "checkProgrammingDependencies.start");
    assert_eq!(request(check), (0x31, &[0x01, 0xFF, 0x01][..], &[][..]));
    let (sid, prefix, fields) = response(check);
    assert_eq!((sid, prefix), (0x71, &[0x01, 0xFF, 0x01][..]));
    assert_eq!(
        (fields[0].short_name.as_deref(), fields[0].bit_offset),
        (Some("result"), 32)
    );
    assert_eq!(
        (
            check.required_session,
            check.p2_millis,
            check.p2_star_millis
        ),
        (0x02, 50, 10_000)
    );
    // Its own NRC first, then the variant-wide ones (4.9).
    assert_eq!(
        negatives(check),
        [(0x24, 14), (0x12, 10), (0x22, 11), (0x31, 12), (0x33, 13)]
    );
}

#[test]
fn shaped_fields_refer_to_the_layouts_they_reach() {
    let documents = documents(&convert(example()));
    let document = &documents[0];
    let records = field(
        response(service(document, "readDtcByStatusMask")).2,
        "records",
    );
    let ShapeT::Repeat(repeat) = &records.shape else {
        panic!("a repeat")
    };
    assert_eq!(
        (repeat.layout_id, repeat.count, repeat.marker.is_none()),
        (1, RepeatCount::UntilEndOfPdu, true)
    );
    let details = field(
        response(service(document, "lastResetReason.read")).2,
        "details",
    );
    let reason = field(
        response(service(document, "lastResetReason.read")).2,
        "reason",
    );
    let ShapeT::Select(select) = &details.shape else {
        panic!("a select")
    };
    assert_eq!(select.key_field_id, reason.id);
    let cases: Vec<(&str, i64, i64, u32)> = select
        .cases
        .iter()
        .flatten()
        .map(|c| {
            (
                c.short_name.as_deref().expect("a name"),
                c.lower,
                c.upper,
                c.layout_id,
            )
        })
        .collect();
    assert_eq!(
        cases,
        [
            ("powerOn", 1, 1, 0),
            ("watchdog", 2, 2, 3),
            ("software", 3, 15, 0)
        ]
    );
    assert!(!select.has_default);
    // dtcStatus is named only by DTCs, so it is not among the layouts (4.10).
    let layouts: Vec<(u32, &str, u32, Vec<FieldT>)> = layouts(document)
        .iter()
        .map(|l| {
            (
                l.id,
                l.short_name.as_deref().expect("a name"),
                l.byte_size,
                l.fields.clone().expect("fields"),
            )
        })
        .collect();
    let id = |name: &str| {
        layouts
            .iter()
            .flat_map(|l| &l.3)
            .find(|f| f.short_name.as_deref() == Some(name))
            .expect("a layout field")
            .id
    };
    assert_eq!(
        layouts,
        [
            (
                1,
                "dtcRecord",
                4,
                vec![
                    scalar(id("dtc"), "dtc", 0, 24, DataKind::Uint),
                    scalar(id("status"), "status", 24, 8, DataKind::Uint),
                ]
            ),
            (
                3,
                "watchdogReset",
                0,
                vec![
                    scalar(id("taskId"), "taskId", 0, 8, DataKind::Uint),
                    scalar(
                        id("programCounter"),
                        "programCounter",
                        8,
                        32,
                        DataKind::Uint
                    ),
                ]
            ),
        ]
    );
}

#[test]
fn variant_patterns_name_the_service_and_field() {
    let documents = documents(&convert(example()));
    for (document, expected) in documents.iter().zip([[0x31, 0x2E], [0x32, 0x2E]]) {
        let version = field(
            response(service(document, "softwareVersion.read")).2,
            "softwareVersion",
        );
        let patterns: Vec<(u32, u32, Vec<u8>)> = variant(document)
            .patterns
            .iter()
            .flatten()
            .map(|p| (p.service_id, p.field_id, p.expected.clone().expect("bytes")))
            .collect();
        assert_eq!(patterns, [(7, version.id, expected.to_vec())]);
    }
}

/// The example with `fields.csv` given a `byte_order` and `max_bytes` column and `rows`
/// appended (in the header order plus those two columns), and the other tables replaced or
/// extended as `with` gives them.
fn with_fields(rows: &[&str]) -> Vec<(String, Vec<u8>)> {
    let fields = example()
        .into_iter()
        .find(|(name, _)| name == "fields.csv")
        .map(|(_, contents)| String::from_utf8(contents).expect("UTF-8"))
        .expect("fields.csv");
    let mut lines: Vec<String> = fields
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("{line},byte_order,max_bytes")
            } else {
                format!("{line},,")
            }
        })
        .collect();
    lines.extend(rows.iter().map(|row| (*row).to_owned()));
    with(example(), "fields.csv", &(lines.join("\n") + "\n"))
}

/// Appends `rows` to the example's `file`.
fn appended(files: Vec<(String, Vec<u8>)>, file: &str, rows: &[&str]) -> Vec<(String, Vec<u8>)> {
    let text = files
        .iter()
        .find(|(name, _)| name == file)
        .map(|(_, contents)| String::from_utf8(contents.clone()).expect("UTF-8"))
        .expect("the file");
    let mut lines: Vec<&str> = text.lines().collect();
    lines.extend(rows);
    with(files, file, &(lines.join("\n") + "\n"))
}

#[test]
fn every_shape_length_and_conversion_kind_converts() {
    // container,name,byte,bit,type,bits,length,min_bytes,terminator,conversion,unit,const,shape,
    // layout,ref,byte_order,max_bytes
    let files = with_fields(&[
        "request:custom,mode,0,,uint,8,,,,,,0x10,,,,,",
        "request:custom,tag,,,bytes,8,leading,,,,,,,,,,",
        "response:custom,mode,0,,uint,8,,,,,,0x10,,,,,",
        "response:custom,count,1,,uint,8,,,,,,,,,,,",
        "response:custom,items,2,,,,,,,,,,repeat,pair,count,,",
        "response:custom,len,,,uint,8,,,,,,,,,,,",
        "response:custom,blob,,,bytes,,field,,,,,,,,len,,",
        "response:custom,temp,,,int,16,,,,scaled,,,,,,little,",
        "response:custom,curve,,,uint,8,,,,curve,,,,,,,",
        "response:custom,ratio,,,uint,8,,,,ratio,,,,,,,",
        "response:custom,block,,,,,,,,,,,group,pair,,,",
        "response:custom,list,,,,,,,,,,,repeat,pair,,,",
        "response:custom,fixedList,,,,,,,,,,,repeat,pair,,,",
        "response:custom,key,,,int,8,,,,,,,,,,,",
        "response:custom,choice,,,,,,,,,,,select,,key,,",
        "response:custom,name,,,ascii,,terminated,2,zero,,,,,,,,16",
        "layout:pair,a,0,,uint,8,,,,,,,,,,,",
        "layout:pair,b,1,,uint,8,,,,,,,,,,,",
    ]);
    let files = appended(files, "services.csv", &["custom,0x23"]);
    let files = appended(files, "layouts.csv", &["pair,2"]);
    let files = with(
        files,
        "repeats.csv",
        "container,field,count,items,item_bytes,marker_bits,marker_lower,marker_upper\n\
         response:readDtcByStatusMask,records,end,,,,,\n\
         response:custom,items,field,,,,,\n\
         response:custom,list,marker,,,16,0xFFFF,\n\
         response:custom,fixedList,fixed,3,2,,,\n",
    );
    let files = appended(
        files,
        "select_cases.csv",
        &[
            "response:custom,choice,negative,-128,-1,pair",
            "response:custom,choice,other,,,",
        ],
    );
    let files = with(
        files,
        "conversions.csv",
        "name,kind,numerator,denominator,offset,default_text,numerator_coeffs,denominator_coeffs\n\
         sessionName,text-table,,,,value.unknown,,\n\
         checkResult,text-table,,,,value.unknown,,\n\
         supplyVoltage,linear,1,10,0,,,\n\
         tenMilliseconds,linear,10,1,0,,,\n\
         scaled,scale-linear,,,,,,\n\
         curve,interpolation,,,,,,\n\
         ratio,rational,,,,,1;2,3\n",
    );
    let files = with(
        files,
        "conversion_scales.csv",
        "conversion,lower,upper,text,numerator,denominator,offset,internal,physical\n\
         sessionName,0x01,,session.default,,,,,\n\
         sessionName,0x02,,session.programming,,,,,\n\
         sessionName,0x03,,session.extended,,,,,\n\
         checkResult,0x00,,check.correct,,,,,\n\
         checkResult,0x01,0xFF,check.incorrect,,,,,\n\
         scaled,-10,0,,2,,,,\n\
         scaled,0.5,,,,4,1,,\n\
         curve,,,,,,,0,0\n\
         curve,,,,,,,255,100\n",
    );
    let documents = documents(&convert(files));
    let document = &documents[0];
    let custom = service(document, "custom");
    let (sid, prefix, params) = request(custom);
    assert_eq!((sid, prefix), (0x23, &[0x10][..]));
    assert_eq!(
        (
            params[0].placement,
            params[0].bit_offset,
            params[0].length_kind,
            params[0].bit_length,
            params[0].kind
        ),
        (
            Placement::AfterPrevious,
            0,
            LengthKind::LeadingLength,
            8,
            DataKind::Bytes
        )
    );
    let (sid, prefix, fields) = response(custom);
    assert_eq!((sid, prefix), (0x63, &[0x10][..]));
    let f = |name| field(fields, name);
    let pair = layouts(document)
        .iter()
        .find(|l| l.short_name.as_deref() == Some("pair"))
        .expect("pair is reached")
        .id;
    let repeat = |name| match &f(name).shape {
        ShapeT::Repeat(r) => r.clone(),
        _ => panic!("{name} is not a repeat"),
    };
    let items = repeat("items");
    assert_eq!(
        (items.layout_id, items.count, items.count_field_id),
        (pair, RepeatCount::FromField, f("count").id)
    );
    let list = repeat("list");
    let marker = list.marker.as_deref().expect("a marker");
    assert_eq!(
        (
            list.count,
            list.marker_lower,
            list.marker_upper,
            marker.short_name.as_deref(),
            marker.bit_length,
            marker.kind,
            marker.placement,
            marker.bit_offset,
        ),
        (
            RepeatCount::UntilMarker,
            0xFFFF,
            0xFFFF,
            Some("list.marker"),
            16,
            DataKind::Uint,
            Placement::Absolute,
            0
        )
    );
    // The marker's id follows every fields.csv id.
    let highest = layouts(document)
        .iter()
        .flat_map(|l| l.fields.iter().flatten())
        .chain(fields)
        .map(|f| f.id)
        .max()
        .expect("fields");
    assert!(marker.id > highest);
    let fixed = repeat("fixedList");
    assert_eq!(
        (fixed.count, fixed.fixed_count, fixed.item_byte_size),
        (RepeatCount::Fixed, 3, 2)
    );
    let ShapeT::Group(group) = &f("block").shape else {
        panic!("a group")
    };
    assert_eq!(group.layout_id, pair);
    assert_eq!(
        (f("blob").length_kind, f("blob").length_field_id),
        (LengthKind::FromField, f("len").id)
    );
    assert_eq!(
        (f("blob").placement, f("len").placement),
        (Placement::AfterPrevious, Placement::AfterPrevious)
    );
    assert_eq!(f("temp").byte_order, ByteOrder::Little);
    let CompuT::CompuScaleLinear(scaled) = &f("temp").compu else {
        panic!("scale-linear")
    };
    let scales: Vec<(f64, f64, f64, f64, f64)> = scaled
        .scales
        .iter()
        .flatten()
        .map(|s| (s.lower, s.upper, s.numerator, s.denominator, s.offset))
        .collect();
    assert_eq!(
        scales,
        [(-10.0, 0.0, 2.0, 1.0, 0.0), (0.5, 0.5, 1.0, 4.0, 1.0)]
    );
    let CompuT::CompuTableInterp(curve) = &f("curve").compu else {
        panic!("interpolation")
    };
    let points: Vec<(f64, f64)> = curve
        .points
        .iter()
        .flatten()
        .map(|p| (p.internal, p.physical))
        .collect();
    assert_eq!(points, [(0.0, 0.0), (255.0, 100.0)]);
    let CompuT::CompuRational(ratio) = &f("ratio").compu else {
        panic!("rational")
    };
    assert_eq!(
        (
            ratio.numerator_coeffs.as_deref(),
            ratio.denominator_coeffs.as_deref()
        ),
        (Some(&[1.0, 2.0][..]), Some(&[3.0][..]))
    );
    let ShapeT::Select(choice) = &f("choice").shape else {
        panic!("a select")
    };
    let cases: Vec<(i64, i64, u32)> = choice
        .cases
        .iter()
        .flatten()
        .map(|c| (c.lower, c.upper, c.layout_id))
        .collect();
    assert_eq!(cases, [(-128, -1, pair)]);
    assert_eq!(
        (
            choice.has_default,
            choice.default_layout_id,
            choice.key_field_id
        ),
        (true, 0, f("key").id)
    );
    let name = f("name");
    assert_eq!(
        (
            name.length_kind,
            name.min_bytes,
            name.max_bytes,
            name.termination
        ),
        (LengthKind::Terminated, 2, 16, Termination::Zero)
    );
    // Shaped fields carry no conversion.
    assert!(matches!(f("block").compu, CompuT::NONE));
}

#[test]
fn unsigned_select_limits_are_stored_bit_for_bit() {
    let files = with_fields(&[
        "response:wide,key,1,,uint,64,,,,,,,,,,,",
        "response:wide,choice,9,,,,,,,,,,select,,key,,",
    ]);
    let files = appended(files, "services.csv", &["wide,0x23"]);
    let files = appended(
        files,
        "select_cases.csv",
        &["response:wide,choice,top,0xFFFFFFFFFFFFFFFF,,"],
    );
    let documents = documents(&convert(files));
    let choice = field(response(service(&documents[0], "wide")).2, "choice");
    let ShapeT::Select(select) = &choice.shape else {
        panic!("a select")
    };
    let case = &select.cases.as_deref().expect("cases")[0];
    assert_eq!((case.lower, case.upper), (-1, -1));
}
