use diag_ir::declaration::{DataKind, FieldT, FlashSessionT, FlashStep, Placement};

use super::services::{appended, with_fields};
use super::*;

/// Each field as (id, name, bit offset, bit length, kind).
fn fields(fields: Option<&Vec<FieldT>>) -> Vec<(u32, &str, u32, u32, DataKind)> {
    fields
        .expect("fields")
        .iter()
        .map(|f| {
            assert_eq!(f.placement, Placement::Absolute);
            (
                f.id,
                f.short_name.as_deref().expect("a name"),
                f.bit_offset,
                f.bit_length,
                f.kind,
            )
        })
        .collect()
}

/// The fields of the example's `dtcStatus` layout: ids 12 to 19, one bit each.
fn status_bits() -> Vec<(u32, &'static str, u32, u32, DataKind)> {
    [
        "failedNow",
        "failedThisCycle",
        "pending",
        "confirmed",
        "untestedSinceClear",
        "failedSinceClear",
        "untestedThisCycle",
        "warningLamp",
    ]
    .into_iter()
    .zip(0..)
    .map(|(name, bit)| (12 + bit, name, bit, 1, DataKind::Uint))
    .collect()
}

/// Each DTC as (code, display code, message id).
fn dtcs(document: &EcuDocumentT) -> Vec<(u32, &str, u32)> {
    variant(document)
        .dtcs
        .iter()
        .flatten()
        .map(|d| {
            (
                d.code,
                d.display_code.as_deref().expect("a display code"),
                d.message_id,
            )
        })
        .collect()
}

fn sessions(document: &EcuDocumentT) -> &[FlashSessionT] {
    variant(document)
        .flash_sessions
        .as_deref()
        .expect("flash sessions")
}

/// A segment as (name, start address, length, artifact offset, block size, checksum).
type SegmentView<'s> = (&'s str, u64, u64, u64, u32, &'s [u8]);

fn segments(session: &FlashSessionT) -> Vec<SegmentView<'_>> {
    session
        .segments
        .iter()
        .flatten()
        .map(|s| {
            (
                s.short_name.as_deref().expect("a name"),
                s.start_address,
                s.length,
                s.artifact_offset,
                s.block_size,
                s.checksum.as_deref().expect("a checksum"),
            )
        })
        .collect()
}

#[test]
fn the_example_dtcs_and_flash_session_convert() {
    for document in documents(&convert(example())) {
        assert_eq!(
            dtcs(&document),
            [(0x91_0116, "B1101-16", 20), (0xC1_0000, "U0100-00", 21)]
        );
        for dtc in variant(&document).dtcs.iter().flatten() {
            assert_eq!(fields(dtc.status_fields.as_ref()), status_bits());
            assert_eq!(fields(dtc.snapshot_fields.as_ref()), []);
            assert_eq!(fields(dtc.extended_fields.as_ref()), []);
        }
        let [session] = sessions(&document) else {
            panic!("one flash session")
        };
        assert_eq!(
            (session.id, session.short_name.as_deref()),
            (1, Some("application"))
        );
        assert_eq!(
            session.steps.as_deref(),
            Some(
                &[
                    FlashStep::EnterProgrammingSession,
                    FlashStep::SecurityAccess,
                    FlashStep::EraseMemory,
                    FlashStep::RequestDownload,
                    FlashStep::TransferData,
                    FlashStep::TransferExit,
                    FlashStep::CheckMemory,
                    FlashStep::EcuReset,
                ][..]
            )
        );
        assert_eq!(segments(session), [("app", 0, 0x1_0000, 0, 0x100, &[][..])]);
    }
}

#[test]
fn dtcs_and_flash_sessions_follow_their_variants_and_layouts() {
    // Ids 34 to 36 follow the example's 33 fields.
    let files = with_fields(&[
        "layout:snap,odometer,0,,uint,16,,,,,,,,,,,",
        "layout:snap,temperature,2,,int,8,,,,,,,,,,,",
        "layout:ext,occurrences,0,,uint,8,,,,,,,,,,,",
    ]);
    let files = appended(files, "layouts.csv", &["snap,3", "ext,0"]);
    let files = with(
        files,
        "dtcs.csv",
        "code,display,text,status,snapshot,extended,variants\n\
         0x910116,B1101-16,dtc.supplyVoltageLow,dtcStatus,snap,ext,\n\
         0xC10000,U0100-00,,dtcStatus,,,exampleEcuV2\n\
         0x123456,P1234-56,dtc.lostCommunication,dtcStatus,snap,,exampleEcuV1\n",
    );
    let files = with(
        files,
        "flash_sessions.csv",
        "id,name,steps,variants\n\
         1,application,request-download;transfer-data;transfer-exit,\n\
         2,calibration,erase-memory;request-download;transfer-data,exampleEcuV2\n",
    );
    let files = with(
        files,
        "flash_segments.csv",
        "session,name,start_address,length,artifact_offset,block_size,checksum\n\
         application,app,0x1000,0x100,0,0x40,A1 B2\n\
         calibration,cal,0x8000,0x10,0x100,0x10,\n\
         application,appTail,0x0,0x100,0x100,0x80,\n",
    );
    let documents = documents(&convert(files));
    let snapshot = [
        (36, "odometer", 0, 16, DataKind::Uint),
        (37, "temperature", 16, 8, DataKind::Int),
    ];
    let extended = [(38, "occurrences", 0, 8, DataKind::Uint)];
    assert_eq!(
        dtcs(&documents[0]),
        [(0x91_0116, "B1101-16", 20), (0x12_3456, "P1234-56", 21)]
    );
    assert_eq!(
        dtcs(&documents[1]),
        [(0x91_0116, "B1101-16", 20), (0xC1_0000, "U0100-00", 0)]
    );
    let all = |document| -> Vec<_> {
        variant(document)
            .dtcs
            .iter()
            .flatten()
            .map(|d| {
                (
                    fields(d.status_fields.as_ref()),
                    fields(d.snapshot_fields.as_ref()),
                    fields(d.extended_fields.as_ref()),
                )
            })
            .collect()
    };
    assert_eq!(
        all(&documents[0]),
        [
            (status_bits(), snapshot.to_vec(), extended.to_vec()),
            (status_bits(), snapshot.to_vec(), vec![]),
        ]
    );
    assert_eq!(
        all(&documents[1]),
        [
            (status_bits(), snapshot.to_vec(), extended.to_vec()),
            (status_bits(), vec![], vec![]),
        ]
    );
    // Layouts only DTCs name are not emitted as layouts (4.10).
    for document in &documents {
        let ids: Vec<u32> = variant(document)
            .layouts
            .iter()
            .flatten()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, [1, 3]);
    }
    let ids = |document| -> Vec<u32> { sessions(document).iter().map(|s| s.id).collect() };
    assert_eq!(ids(&documents[0]), [1]);
    assert_eq!(ids(&documents[1]), [1, 2]);
    let application = &sessions(&documents[1])[0];
    assert_eq!(
        application.steps.as_deref(),
        Some(
            &[
                FlashStep::RequestDownload,
                FlashStep::TransferData,
                FlashStep::TransferExit
            ][..]
        )
    );
    assert_eq!(
        segments(application),
        [
            ("app", 0x1000, 0x100, 0, 0x40, &[0xA1, 0xB2][..]),
            ("appTail", 0, 0x100, 0x100, 0x80, &[][..]),
        ]
    );
    assert_eq!(
        segments(&sessions(&documents[1])[1]),
        [("cal", 0x8000, 0x10, 0x100, 0x10, &[][..])]
    );
    assert_eq!(sessions(&documents[0])[0], *application);
}
