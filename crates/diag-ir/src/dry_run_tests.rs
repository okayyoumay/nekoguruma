use super::*;
use crate::declaration::{
    EcuDocumentT, EcuVariantT, FieldT, GroupT, LayoutT, PositiveResponseT, RepeatT, RequestT,
    ServiceT, ShapeT, encode_ecu_document, load_ecu_document,
};
use crate::decode::PhysicalValue;
use crate::{IR_SCHEMA_VERSION, Op};

/// A variant with `services` and `layouts`, encoded and loaded through the checking loader.
fn document(services: Vec<ServiceT>, layouts: Vec<LayoutT>) -> Vec<u8> {
    encode_ecu_document(&EcuDocumentT {
        schema_version: IR_SCHEMA_VERSION,
        source_format: Some("csv-starlark".into()),
        source_digest: Some("00".into()),
        variant: Some(Box::new(EcuVariantT {
            id: 1,
            services: Some(services),
            layouts: Some(layouts),
            ..Default::default()
        })),
        procedure: None,
    })
}

/// A service `id` named `name` whose request is `sid` and `prefix`, answered by `sid + 0x40`
/// with no prefix and `fields`.
fn service(id: u32, name: &str, sid: u8, prefix: &[u8], fields: Vec<FieldT>) -> ServiceT {
    ServiceT {
        id,
        short_name: Some(name.into()),
        request: Some(Box::new(RequestT {
            sid,
            prefix: Some(prefix.to_vec()),
            params: None,
        })),
        positive: Some(Box::new(PositiveResponseT {
            sid: sid + 0x40,
            prefix: Some(Vec::new()),
            fields: Some(fields),
        })),
        ..Default::default()
    }
}

/// A `Fixed` field at an absolute bit offset.
fn at(id: u32, kind: DataKind, bit_offset: u32, bit_length: u32) -> FieldT {
    FieldT {
        id,
        short_name: Some(format!("f{id}")),
        bit_offset,
        bit_length,
        kind,
        ..Default::default()
    }
}

/// A `Fixed` field that follows the previous one.
fn next(id: u32, kind: DataKind, bit_length: u32) -> FieldT {
    FieldT {
        placement: Placement::AfterPrevious,
        ..at(id, kind, 0, bit_length)
    }
}

fn terminated(id: u32, kind: DataKind, termination: Termination, min: u32, max: u32) -> FieldT {
    FieldT {
        length_kind: LengthKind::Terminated,
        termination,
        min_bytes: min,
        max_bytes: max,
        ..next(id, kind, 0)
    }
}

/// The answer built for the only service of a document with `fields` and `layouts`, and what
/// it decodes to.
fn built(
    fields: Vec<FieldT>,
    layouts: Vec<LayoutT>,
) -> (Vec<u8>, Result<Vec<PhysicalValue>, DecodeError>) {
    let buf = document(vec![service(1, "s", 0x22, &[], fields)], layouts);
    let loaded = load_ecu_document(&buf).expect("test document loads");
    let variant = loaded.variant().expect("a variant");
    let positive = variant
        .services()
        .and_then(|s| s.get(0).positive())
        .expect("a positive response");
    let answer = built_response(&positive, variant.layouts());
    let decoded = decode_positive_response(&positive, variant.layouts(), &answer)
        .map(|fields| fields.into_iter().map(|f| f.value).collect());
    (answer, decoded)
}

fn program(code: Vec<Op>, constants: Vec<Vec<u8>>) -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants,
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

fn run(buf: &[u8], program: &Program, script: &Script) -> DryRunReport {
    let loaded = load_ecu_document(buf).expect("test document loads");
    dry_run(
        program,
        &loaded.variant().expect("a variant"),
        script,
        10_000,
    )
}

use PhysicalValue::{Bytes, Text, Unsigned};

// ---------------------------------------------------------------- Built answers

#[test]
fn fixed_fields_are_built_as_zeros_up_to_the_last_one() {
    let fields = vec![
        next(1, DataKind::Uint, 16),
        // A 4-bit field 3 bits into the byte after the first, then one after it.
        FieldT {
            bit_offset: 3,
            ..next(2, DataKind::Uint, 4)
        },
        next(3, DataKind::Uint, 8),
        at(4, DataKind::Uint, 8 * 6, 8),
    ];
    let (answer, decoded) = built(fields, vec![]);
    assert_eq!(answer, [0x62, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        decoded,
        Ok(vec![Unsigned(0), Unsigned(0), Unsigned(0), Unsigned(0)])
    );
}

#[test]
fn a_terminated_string_gets_its_least_bytes_and_a_terminator() {
    let fields = vec![
        terminated(1, DataKind::Ascii, Termination::Zero, 1, 0),
        terminated(2, DataKind::Bytes, Termination::HexFF, 0, 4),
        terminated(3, DataKind::Unicode2, Termination::HexFF, 2, 0),
        next(4, DataKind::Uint, 8),
    ];
    let (answer, decoded) = built(fields, vec![]);
    assert_eq!(answer, [0x62, 0, 0, 0xFF, 0, 0, 0xFF, 0xFF, 0]);
    assert_eq!(
        decoded,
        Ok(vec![
            Text("\0".into()),
            Bytes(vec![]),
            Text("\0".into()),
            Unsigned(0)
        ])
    );
}

#[test]
fn a_terminated_string_with_no_room_for_a_terminator_takes_max_bytes() {
    let fields = vec![
        terminated(1, DataKind::Bytes, Termination::Zero, 2, 2),
        terminated(2, DataKind::Bytes, Termination::EndOfPdu, 1, 3),
        next(3, DataKind::Uint, 8),
    ];
    let (answer, decoded) = built(fields, vec![]);
    assert_eq!(answer.len(), 1 + 2 + 3 + 1);
    assert_eq!(
        decoded,
        Ok(vec![Bytes(vec![0, 0]), Bytes(vec![0, 0, 0]), Unsigned(0)])
    );
}

#[test]
fn a_value_that_reads_to_the_end_ends_the_built_answer() {
    let fields = vec![
        terminated(1, DataKind::Bytes, Termination::EndOfPdu, 2, 0),
        next(2, DataKind::Uint, 8),
    ];
    let (answer, decoded) = built(fields, vec![]);
    assert_eq!(answer, [0x62, 0, 0]);
    // The field after it has no bytes left: it needs a scripted answer.
    assert!(decoded.is_err(), "{decoded:?}");
}

#[test]
fn leading_and_field_lengths_are_built_as_zero() {
    let fields = vec![
        FieldT {
            length_kind: LengthKind::LeadingLength,
            ..next(1, DataKind::Ascii, 8)
        },
        next(2, DataKind::Uint, 8),
        FieldT {
            length_kind: LengthKind::FromField,
            length_field_id: 2,
            ..next(3, DataKind::Bytes, 0)
        },
        next(4, DataKind::Uint, 16),
    ];
    let (answer, decoded) = built(fields, vec![]);
    assert_eq!(answer, [0x62, 0, 0, 0, 0]);
    assert_eq!(
        decoded,
        Ok(vec![
            Text(String::new()),
            Unsigned(0),
            Bytes(vec![]),
            Unsigned(0)
        ])
    );
}

#[test]
fn groups_and_repeats_of_known_size_are_built() {
    let layout = |id: u32, byte_size: u32| LayoutT {
        id,
        short_name: Some(format!("l{id}")),
        byte_size,
        fields: Some(vec![at(10 + id, DataKind::Uint, 0, 8)]),
    };
    let shaped = |id: u32, shape: ShapeT| FieldT {
        shape,
        ..next(id, DataKind::Bytes, 0)
    };
    let fields = vec![
        shaped(1, ShapeT::Group(Box::new(GroupT { layout_id: 1 }))),
        shaped(
            2,
            ShapeT::Repeat(Box::new(RepeatT {
                layout_id: 2,
                count: RepeatCount::Fixed,
                fixed_count: 2,
                item_byte_size: 3,
                ..Default::default()
            })),
        ),
        next(3, DataKind::Uint, 8),
        shaped(
            4,
            ShapeT::Repeat(Box::new(RepeatT {
                layout_id: 2,
                count: RepeatCount::UntilEndOfPdu,
                ..Default::default()
            })),
        ),
    ];
    let (answer, decoded) = built(fields, vec![layout(1, 2), layout(2, 3)]);
    assert_eq!(answer.len(), 1 + 2 + 6 + 1);
    let values = decoded.expect("the built answer decodes");
    assert_eq!(values.len(), 4);
    assert_eq!(values[2], Unsigned(0));
}

#[test]
fn a_field_placed_far_out_is_capped() {
    let (answer, _) = built(vec![at(1, DataKind::Uint, u32::MAX - 8, 8)], vec![]);
    assert_eq!(answer.len(), crate::MAX_BYTES_LEN);
}

// ---------------------------------------------------------------- Resolving requests

#[test]
fn the_longest_matching_prefix_decides_and_a_tie_is_ambiguous() {
    let buf = document(
        vec![
            service(1, "any", 0x31, &[], vec![]),
            service(2, "start", 0x31, &[0x01], vec![]),
            service(3, "startFf01", 0x31, &[0x01, 0xFF, 0x01], vec![]),
            service(4, "a", 0x22, &[0xF1], vec![]),
            service(5, "b", 0x22, &[0xF1], vec![]),
        ],
        vec![],
    );
    let routine = |routine: u16| {
        program(
            vec![Op::PushBytes(0), Op::RoutineControl { routine, sub: 0x01 }],
            vec![vec![]],
        )
    };
    let services = |report: &DryRunReport| -> Vec<String> {
        report
            .events
            .iter()
            .filter_map(|e| match e {
                DryRunEvent::Request { service, .. } => Some(service.clone()),
                _ => None,
            })
            .collect()
    };
    let report = run(&buf, &routine(0xFF01), &Script::new());
    assert_eq!(report.end, DryRunEnd::Finished, "{report:?}");
    assert_eq!(services(&report), ["startFf01"]);
    let report = run(&buf, &routine(0xFF00), &Script::new());
    assert_eq!(services(&report), ["start"]);

    let read = program(
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        vec![vec![0xF1, 0x90]],
    );
    let report = run(&buf, &read, &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::Ambiguous {
            request: vec![0x22, 0xF1, 0x90],
            services: vec!["a".into(), "b".into()],
        })
    );

    let wide = program(
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x122 }],
        vec![vec![0xF1, 0x90]],
    );
    let report = run(&buf, &wide, &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::NotASid { service: 0x122 })
    );
}

#[test]
fn a_send_receive_must_start_with_its_services_sid() {
    let buf = document(vec![service(7, "vin", 0x22, &[0xF1, 0x90], vec![])], vec![]);
    let send = |request: Vec<u8>| {
        program(
            vec![Op::PushBytes(0), Op::SendReceive { service: 7 }],
            vec![request],
        )
    };
    let report = run(&buf, &send(vec![0x22, 0xF1, 0x90]), &Script::new());
    assert_eq!(report.end, DryRunEnd::Finished, "{report:?}");
    assert!(matches!(
        &report.events[..],
        [DryRunEvent::Request { service, answer, built: true, .. }]
            if service == "vin" && answer == &[0x62]
    ));
    for request in [vec![], vec![0x2E, 0xF1, 0x90]] {
        let report = run(&buf, &send(request.clone()), &Script::new());
        assert_eq!(
            report.end,
            DryRunEnd::Host(DryRunError::WrongService {
                service: "vin".into(),
                sid: 0x22,
                request,
            })
        );
    }
}

// ---------------------------------------------------------------- Other primitives

#[test]
fn keys_and_records_come_from_the_script() {
    let buf = document(vec![], vec![]);
    let code = vec![
        Op::PushBytes(0),
        Op::SecurityAccess { level: 1 },
        Op::Pop,
        Op::RecordInput { template: 1 },
        Op::Pop,
    ];
    let template = crate::hmi::HmiTemplate {
        id: "template".to_owned(),
        params: Vec::new(),
    };
    let constants = vec![
        vec![0xAA],
        crate::hmi::encode_template(&template).expect("encodes"),
    ];
    let report = run(
        &buf,
        &program(code.clone(), constants.clone()),
        &Script::new(),
    );
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::NotScripted { what: "key" })
    );
    let script = Script::new().key(vec![0x55]).record_answer(true, "ok");
    let report = run(&buf, &program(code, constants), &script);
    assert_eq!(report.end, DryRunEnd::Finished, "{report:?}");
    assert_eq!(
        report.events,
        [
            DryRunEvent::SecurityAccess {
                level: 1,
                seed: vec![0xAA],
                key: vec![0x55]
            },
            DryRunEvent::RecordInput {
                template: crate::hmi::HmiForm {
                    id: "template".to_owned(),
                    params: Vec::new(),
                },
                answer: b"\x01ok".to_vec()
            }
        ]
    );
}

#[test]
fn the_report_stops_growing_at_its_budget() {
    let buf = document(vec![service(7, "big", 0x2E, &[], vec![])], vec![]);
    let mut request = vec![0x2E];
    request.resize(1 << 20, 0);
    // Each round reports the 1 MiB request, its 1-byte answer and the service's name.
    let program = program(
        vec![
            Op::PushBytes(0),
            Op::SendReceive { service: 7 },
            Op::Pop,
            Op::Jump(0),
        ],
        vec![request],
    );
    let report = run(&buf, &program, &Script::new());
    assert_eq!(report.end, DryRunEnd::Host(DryRunError::ReportTooLarge));
    assert_eq!(
        report.events.len(),
        MAX_REPORT_BYTES / ((1 << 20) + 1 + "big".len() + EVENT_COST)
    );
}

#[test]
fn a_repeat_too_large_to_count_is_capped() {
    let layout = LayoutT {
        id: 1,
        short_name: Some("l1".into()),
        byte_size: 0,
        fields: Some(vec![at(11, DataKind::Uint, 0, 8)]),
    };
    let field = FieldT {
        shape: ShapeT::Repeat(Box::new(RepeatT {
            layout_id: 1,
            count: RepeatCount::Fixed,
            fixed_count: u32::MAX,
            item_byte_size: u32::MAX,
            ..Default::default()
        })),
        ..next(1, DataKind::Bytes, 0)
    };
    let (answer, _) = built(vec![field, next(2, DataKind::Uint, 8)], vec![layout]);
    assert_eq!(answer.len(), crate::MAX_BYTES_LEN);
}

#[test]
fn a_number_whose_length_is_another_fields_ends_the_built_answer() {
    let fields = vec![
        next(1, DataKind::Uint, 8),
        FieldT {
            length_kind: LengthKind::FromField,
            length_field_id: 1,
            ..next(2, DataKind::Uint, 0)
        },
        next(3, DataKind::Uint, 8),
    ];
    let (answer, decoded) = built(fields, vec![]);
    // A width of zero is no number, so the answer stops before it and needs a script.
    assert_eq!(answer, [0x62, 0]);
    assert!(decoded.is_err(), "{decoded:?}");
}

#[test]
fn decoded_values_count_toward_the_budget() {
    // 4096 fields that all read the same 256 bytes: a 257-byte answer decodes to 1 MiB.
    let fields: Vec<FieldT> = (1..=4096)
        .map(|id| at(id, DataKind::Bytes, 8, 8 * 256))
        .collect();
    let buf = document(vec![service(7, "wide", 0x22, &[], fields)], vec![]);
    let program = program(
        vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::Pop,
            Op::Jump(0),
        ],
        vec![vec![]],
    );
    let report = run(&buf, &program, &Script::new());
    assert_eq!(report.end, DryRunEnd::Host(DryRunError::ReportTooLarge));
    assert!(
        report.events.len() < MAX_REPORT_BYTES / (4096 * 256),
        "{} events",
        report.events.len()
    );
}

#[test]
fn a_last_log_past_the_budget_still_ends_the_run() {
    let buf = document(vec![service(7, "big", 0x2E, &[], vec![])], vec![]);
    let mut request = vec![0x2E];
    request.resize(MAX_REPORT_BYTES - 2 * EVENT_COST, 0);
    let program = program(
        vec![
            Op::PushBytes(0),
            Op::SendReceive { service: 7 },
            Op::Pop,
            Op::Log {
                level: 1,
                message: 1,
            },
        ],
        vec![request, b"a message past the budget".to_vec()],
    );
    let report = run(&buf, &program, &Script::new());
    assert_eq!(report.end, DryRunEnd::Host(DryRunError::ReportTooLarge));
    assert_eq!(report.events.len(), 1);
}

/// A log whose message is a run-time operand is charged by the text the host receives: invalid
/// UTF-8 decodes to longer text, and a message past the budget ends the run after its step.
#[test]
fn a_run_time_log_message_counts_toward_the_budget() {
    let buf = document(vec![], vec![]);
    let program = program(
        vec![
            Op::PushBytes(0),
            Op::LogFromStack { level: 2 },
            Op::PushBytes(1),
            Op::LogFromStack { level: 3 },
            Op::Wait { millis: 1 },
        ],
        vec![vec![b'o', 0xFF, b'k'], vec![0xFF; MAX_REPORT_BYTES / 2]],
    );
    let report = run(&buf, &program, &Script::new());
    // Each invalid byte becomes three bytes of text, so the second message is past the budget.
    assert_eq!(report.end, DryRunEnd::Host(DryRunError::ReportTooLarge));
    assert_eq!(
        report.events,
        vec![DryRunEvent::Log {
            level: 2,
            message: "o\u{FFFD}k".to_owned()
        }]
    );
}

#[test]
fn events_without_bytes_count_their_own_size() {
    let buf = document(vec![], vec![]);
    let program = program(vec![Op::Wait { millis: 1 }, Op::Jump(0)], Vec::new());
    let loaded = load_ecu_document(&buf).expect("test document loads");
    let report = dry_run(
        &program,
        &loaded.variant().expect("a variant"),
        &Script::new(),
        u64::MAX,
    );
    assert_eq!(report.end, DryRunEnd::Host(DryRunError::ReportTooLarge));
    assert_eq!(report.events.len(), MAX_REPORT_BYTES / EVENT_COST);
    assert!(report.events.capacity() * size_of::<DryRunEvent>() <= MAX_REPORT_BYTES);
}

#[test]
fn an_error_keeps_only_the_start_of_a_long_request() {
    let buf = document(vec![], vec![]);
    let mut request = vec![0x22];
    request.resize(1 << 20, 0xAB);
    let program = program(
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        vec![request[1..].to_vec()],
    );
    let report = run(&buf, &program, &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::Undeclared {
            request: request[..MAX_ERROR_BYTES].to_vec()
        })
    );
}

#[test]
fn an_error_keeps_only_the_start_of_a_long_service_name() {
    let name = "€".repeat(1000);
    let bare = ServiceT {
        positive: None,
        ..service(7, &name, 0x22, &[], vec![])
    };
    let buf = document(vec![bare], vec![]);
    let program = program(
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        vec![vec![]],
    );
    let report = run(&buf, &program, &Script::new());
    // "€" is three bytes, so the name is cut back to the character boundary at 63 bytes.
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::NoPositiveResponse {
            service: "€".repeat(MAX_ERROR_BYTES / 3)
        })
    );
}

#[test]
fn no_field_after_one_of_unknown_extent_adds_bytes() {
    let layout = LayoutT {
        id: 1,
        short_name: Some("l1".into()),
        byte_size: 0,
        fields: Some(vec![at(11, DataKind::Uint, 0, 8)]),
    };
    let fields = vec![
        next(1, DataKind::Uint, 8),
        FieldT {
            shape: ShapeT::Group(Box::new(GroupT { layout_id: 1 })),
            ..next(2, DataKind::Bytes, 0)
        },
        // Placed absolutely, it would otherwise give the group a byte to decode from.
        at(3, DataKind::Uint, 8 * 4, 8),
    ];
    let (answer, decoded) = built(fields, vec![layout]);
    assert_eq!(answer, [0x62, 0]);
    assert!(decoded.is_err(), "{decoded:?}");
}

// ---------------------------------------------------------------- FlashBlock (ADR-336 item 15)

/// A document with flash session 2 holding `segment`, and the services a plan's erase,
/// download and transfer exit send (any answer will do).
fn flash_document(segment: crate::declaration::FlashSegmentT) -> Vec<u8> {
    use crate::declaration::FlashSessionT;
    encode_ecu_document(&EcuDocumentT {
        schema_version: IR_SCHEMA_VERSION,
        source_format: Some("csv-starlark".into()),
        source_digest: Some("00".into()),
        variant: Some(Box::new(EcuVariantT {
            id: 1,
            services: Some(vec![
                service(1, "erase", 0x31, &[], vec![]),
                service(2, "download", 0x34, &[], vec![]),
                service(3, "exit", 0x37, &[], vec![]),
            ]),
            flash_sessions: Some(vec![FlashSessionT {
                id: 2,
                segments: Some(vec![segment]),
                ..Default::default()
            }]),
            ..Default::default()
        })),
        procedure: None,
    })
}

/// A program of flash session 2 whose plan starts with `ops`, which `FlashBlock` must sit in
/// (`Program::validate`), then erases, requests the download and exits the transfer.
fn in_plan(mut ops: Vec<Op>) -> Program {
    use crate::{
        FlashRecovery, IdentitySources, NoApplication, RecoveryBoundaries, RecoveryRequired,
        RecoveryTiming, Source,
    };
    let erase = ops.len() as u32 + 1;
    ops.extend([
        Op::PushBytes(0),
        Op::RoutineControl { routine: 1, sub: 1 },
        Op::Pop,
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x34 },
        Op::Pop,
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x37 },
        Op::Pop,
    ]);
    let mut program = program(ops, vec![vec![]]);
    let source = Some(Source::EcuService {
        service_id: 1,
        field_id: 1,
    });
    program.identity = IdentitySources {
        vin: source,
        hardware_part_number: source,
        software_version: source,
    };
    program.flash.push(FlashRecovery {
        flash_session: 2,
        stage: 1,
        max_resumes: 3,
        recovery_required: RecoveryRequired::Never,
        boundaries: RecoveryBoundaries {
            entry_pc: 0,
            erase_pc: erase,
            transfer_exit_pc: erase + 6,
            post_transfer_end_pc: erase + 8,
        },
        timing: RecoveryTiming {
            session_timeout_millis: 5000,
            teardown_margin_millis: 500,
            ecu_startup_millis: 2000,
            confirmation_window_millis: 10_000,
        },
        version_read_retries: 2,
        no_application: Some(NoApplication::Nrc(0x22)),
    });
    program
}

#[test]
fn a_flash_block_is_served_as_zeros_of_the_blocks_length() {
    let buf = flash_document(crate::declaration::FlashSegmentT {
        artifact_offset: 16,
        length: 10,
        block_size: 4,
        ..Default::default()
    });
    // The last block has two bytes; the log shows what the block held.
    let program = in_plan(vec![
        Op::PushI64(2),
        Op::FlashBlock {
            session: 2,
            segment: 0,
        },
        Op::LogFromStack { level: 1 },
    ]);
    let report = run(&buf, &program, &Script::new());
    assert_eq!(report.end, DryRunEnd::Finished, "{report:?}");
    assert_eq!(
        report.events.first(),
        Some(&DryRunEvent::Log {
            level: 1,
            message: "\0\0".into()
        })
    );
}

#[test]
fn a_flash_block_outside_the_declarations_ends_the_run_with_the_vms_error() {
    let buf = flash_document(crate::declaration::FlashSegmentT {
        length: 10,
        block_size: 4,
        ..Default::default()
    });
    let program = in_plan(vec![
        Op::PushI64(3),
        Op::FlashBlock {
            session: 2,
            segment: 0,
        },
    ]);
    let report = run(&buf, &program, &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Vm(VmError::BlockOutOfRange {
            session: 2,
            segment: 0,
            index: 3
        })
    );
}

#[test]
fn flash_blocks_count_toward_the_budget() {
    let buf = flash_document(crate::declaration::FlashSegmentT {
        length: 1 << 20,
        block_size: 1 << 20,
        ..Default::default()
    });
    let program = in_plan(vec![
        Op::PushI64(0),
        Op::FlashBlock {
            session: 2,
            segment: 0,
        },
        Op::Pop,
        Op::Jump(0),
    ]);
    let report = run(&buf, &program, &Script::new());
    assert_eq!(report.end, DryRunEnd::Host(DryRunError::ReportTooLarge));
}

#[test]
fn a_flash_block_outside_a_plan_is_invalid() {
    let buf = flash_document(crate::declaration::FlashSegmentT {
        length: 10,
        block_size: 4,
        ..Default::default()
    });
    let program = program(
        vec![
            Op::PushI64(0),
            Op::FlashBlock {
                session: 2,
                segment: 0,
            },
        ],
        vec![],
    );
    let report = run(&buf, &program, &Script::new());
    assert_eq!(
        report.end,
        DryRunEnd::Invalid(crate::ProgramError::FlashBlockOutsidePlan { pc: 1 })
    );
}

// ---------------------------------------------------------------- ReadInput (ADR-338)

#[test]
fn a_read_input_gives_the_scripted_value_and_leaves_no_event() {
    use crate::RuntimeInput::{IgnitionOn, SupplyVoltageMillivolts};
    let buf = document(Vec::new(), Vec::new());
    let program = program(
        vec![
            Op::ReadInput {
                input: SupplyVoltageMillivolts,
            },
            Op::ReadInput { input: IgnitionOn },
            Op::CmpGt,
        ],
        Vec::new(),
    );
    let script = Script::new()
        .input(SupplyVoltageMillivolts, 12_000)
        .input(IgnitionOn, 0)
        // A later value for the same input replaces the earlier one.
        .input(SupplyVoltageMillivolts, 13_000);
    let report = run(&buf, &program, &script);
    assert_eq!(report.end, DryRunEnd::Finished);
    assert_eq!(report.events, []);
    assert_eq!(report.steps, 3);
}

#[test]
fn an_input_the_script_does_not_set_is_not_established() {
    use crate::RuntimeInput::{EngineRunning, IgnitionOn};
    let buf = document(Vec::new(), Vec::new());
    let program = program(
        vec![Op::ReadInput {
            input: EngineRunning,
        }],
        Vec::new(),
    );
    let report = run(&buf, &program, &Script::new().input(IgnitionOn, 1));
    assert_eq!(
        report.end,
        DryRunEnd::Host(DryRunError::NotScripted {
            what: "value for the runtime input"
        })
    );
    assert_eq!(report.steps, 0);
}
