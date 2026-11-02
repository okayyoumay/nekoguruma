// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! An agent job end to end without hardware (ADR-235, ADR-247): `worker-host` launches the real
//! `j2534-0404-service` binary against the `sim-vci` cdylib, and `agent::run_program` runs a
//! diag-ir procedure on a CAN link to the simulated ECU behind it.
//!
//! The `FlashBlock` and `FlashTransfer` instructions against the simulated ECU (ADR-250,
//! ADR-336): the job's document declares a flash session whose segments are ranges of an
//! artifact file, the job names the file by its SHA-256, and the agent reads each block from the
//! file's bytes with `FlashBlock` and sends it with `FlashTransfer`. A download of more blocks
//! than the wire block counter holds before it wraps, the last one as long as the ECU's
//! maxNumberOfBlockLength allows, followed by RequestTransferExit, which the ECU accepts only
//! when every block arrived in order. A second job's block one byte longer than that (a segment
//! with a larger block size) is refused by the host before it is sent (ADR-336 item 12). (A
//! `FlashTransfer` with no RequestDownload before it cannot pass the program validator, so the
//! host's refusal is a unit test in `crates/agent/src/host.rs`.)
//!
//! A job whose artifact does not hash to the name it gives, or whose segment lies outside the
//! file, is refused before a worker is asked anything; those checks are unit tests in
//! `crates/agent/src/runner.rs`.
//!
//! The jobs run with the write-job journal (ADR-252): the ECU's part number and software
//! version are read at the plan's entry, and the first job's journal reads back with every
//! block, the RequestTransferExit marker and the post-transfer completion. The first job's
//! communication log, beside its journal, holds the VCI's connection with `sim-vci`'s J2534
//! versions and each identity value as it was read (design 8.11 "Header").
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` it sets for the
//! spawned service cannot race with another test; both jobs run in it, one after the other, on
//! one worker.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::inputs::ServiceSources;
use agent::journal::{JobKey, Journal};
use agent::launch::Api;
use agent::{
    Artifact, HostError, Job, JobDocument, JobError, JobLimits, JobRun, JournalSetup, LinkConfig,
    VciFacts, run_program_journaled,
};
use diag_ir::declaration::{
    DataKind, EcuDocumentT, EcuVariantT, FieldT, FlashSegmentT, FlashSessionT, Placement,
    PositiveResponseT, ProtocolT, RequestT, ServiceT, encode_ecu_document,
};
use diag_ir::{
    FlashRecovery, IR_SCHEMA_VERSION, IdentitySources, Op, Program, RecoveryBoundaries,
    RecoveryRequired, RecoveryTiming, Source, Value,
};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's erase-memory routine (`sim_ecu::RID_ERASE_MEMORY`).
const RID_ERASE_MEMORY: u16 = 0xFF00;
/// More one-byte blocks than the counter holds before it wraps (255), and past its second wrap
/// point of 0x00 (block 256) so the counter is 0x00 and 0x01 again.
const BLOCKS: u32 = 300;
/// The data bytes of a TransferData request as long as the simulated ECU's
/// maxNumberOfBlockLength (`sim_ecu::MAX_BLOCK_LENGTH`, 0x0102), which counts the SID and the
/// counter too.
const LONGEST_BLOCK: usize = 0x0100;
/// The simulated ECU's part number and software version (`sim_ecu::DID_SPARE_PART_NUMBER`,
/// `sim_ecu::DID_SOFTWARE_VERSION`), as the ECU config below sets them.
const PART_NUMBER: &[u8] = b"NGR-SIM-ECU";
const SW_VERSION: &[u8] = b"1.0.0";

/// `sim-vci` is a dev-dependency, so cargo builds its cdylib into the same `deps` directory
/// as this test executable.
fn sim_vci_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe.parent().expect("test executable has a directory");
    let name = format!(
        "{}sim_vci{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = deps.join(&name);
    assert!(path.is_file(), "{} should be built", path.display());
    path
}

/// Width of J2534 `unsigned long` in `sim-vci` on this platform.
fn long_size() -> u8 {
    if cfg!(windows) {
        4
    } else {
        std::mem::size_of::<std::os::raw::c_ulong>() as u8
    }
}

/// Removes the temporary config file however the test ends.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The flash session of the job's document, which the plan names.
const FLASH_SESSION: u32 = 1;

/// A segment of the artifact: `length` bytes from `artifact_offset`, read in blocks of
/// `block_size`.
#[derive(Clone, Copy)]
struct Segment {
    artifact_offset: u64,
    length: u64,
    block_size: u32,
}

impl Segment {
    fn blocks(&self) -> u64 {
        self.length.div_ceil(u64::from(self.block_size))
    }
}

/// Programming session, erase, RequestDownload of the segments' total size at 0, then for every
/// block of every segment a `FlashBlock` (the block's index in its segment) and a
/// `FlashTransfer`, and RequestTransferExit. The `FlashTransfer` operand is the block number the
/// compiler would write; the host ignores it.
fn download_program(segments: &[Segment]) -> Program {
    let size = segments.iter().map(|s| s.length).sum::<u64>() as u32;
    let constants = vec![
        vec![0x02],
        Vec::new(),
        // dataFormatIdentifier, address and size lengths (4 and 4), address 0, size.
        [&[0x00, 0x44, 0, 0, 0, 0][..], &size.to_be_bytes()].concat(),
        Vec::new(),
    ];
    let mut code = vec![
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x10 },
        Op::PushBytes(1),
        Op::RoutineControl {
            routine: RID_ERASE_MEMORY,
            sub: 0x01,
        },
        Op::PushBytes(2),
        Op::ServiceRequest { service: 0x34 },
    ];
    for (segment, declared) in segments.iter().enumerate() {
        for index in 0..declared.blocks() {
            code.push(Op::PushI64(index as i64));
            code.push(Op::FlashBlock {
                session: FLASH_SESSION,
                segment: segment as u32,
            });
            code.push(Op::FlashTransfer { block: 7 });
        }
    }
    code.push(Op::PushBytes(3));
    let exit_pc = code.len() as u32;
    code.push(Op::ServiceRequest { service: 0x37 });
    // The validator wants download instructions inside a flash session's plan. The plan never
    // allows a restart (a recovery point at the erase), so it declares nothing else.
    let flash = vec![FlashRecovery {
        flash_session: FLASH_SESSION,
        stage: 1,
        max_resumes: 1,
        recovery_required: RecoveryRequired::FromPc(3),
        boundaries: RecoveryBoundaries {
            entry_pc: 0,
            erase_pc: 3,
            transfer_exit_pc: exit_pc,
            post_transfer_end_pc: exit_pc + 1,
        },
        timing: RecoveryTiming {
            session_timeout_millis: 5_000,
            teardown_margin_millis: 1_000,
            ecu_startup_millis: 1_000,
            confirmation_window_millis: 1_000,
        },
        version_read_retries: 0,
        no_application: None,
    }];
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants,
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: IdentitySources {
            vin: None,
            hardware_part_number: Some(Source::EcuService {
                service_id: 1,
                field_id: 1,
            }),
            software_version: Some(Source::EcuService {
                service_id: 2,
                field_id: 1,
            }),
        },
        preconditions: Default::default(),
        flash,
        targets: Vec::new(),
    }
}

/// Service `id` of the job's document: ReadDataByIdentifier of `did`, whose response echoes the
/// identifier and then has one ASCII field of `length` bytes. The job reads its identity
/// through these, so the caller gives no source table (ADR-345).
fn read_service(id: u32, did: u16, length: usize) -> ServiceT {
    let did = did.to_be_bytes().to_vec();
    let mut field = FieldT::default();
    field.id = 1;
    field.short_name = Some("value".into());
    field.placement = Placement::AfterPrevious;
    field.bit_length = u32::try_from(length * 8).expect("a short field");
    field.kind = DataKind::Ascii;
    let mut request = RequestT::default();
    request.sid = 0x22;
    request.prefix = Some(did.clone());
    let mut positive = PositiveResponseT::default();
    positive.sid = 0x62;
    positive.prefix = Some(did);
    positive.fields = Some(vec![field]);
    let mut service = ServiceT::default();
    service.id = id;
    service.short_name = Some(format!("read{id}"));
    service.request = Some(Box::new(request));
    service.positive = Some(Box::new(positive));
    service
}

/// One-byte blocks past the counter's wrap (segment 0), then one block as long as the ECU
/// allows (segment 1), right after them in the artifact.
fn accepted_segments() -> Vec<Segment> {
    vec![
        Segment {
            artifact_offset: 0,
            length: u64::from(BLOCKS),
            block_size: 1,
        },
        Segment {
            artifact_offset: u64::from(BLOCKS),
            length: LONGEST_BLOCK as u64,
            block_size: LONGEST_BLOCK as u32,
        },
    ]
}

/// One byte more than the ECU allows in one block: a single block of a segment whose block
/// size is that long.
fn long_block_segments() -> Vec<Segment> {
    vec![Segment {
        artifact_offset: 0,
        length: LONGEST_BLOCK as u64 + 1,
        block_size: LONGEST_BLOCK as u32 + 1,
    }]
}

/// The artifact's bytes: a pattern that differs from block to block, so a block read from the
/// wrong place would differ.
fn artifact_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// The job: a document whose variant declares the flash session with `segments` and whose
/// procedure is [`download_program`], and the artifact file at `artifact_path` that it names by
/// the file's SHA-256.
fn download_job(segments: &[Segment], artifact_path: &std::path::Path) -> Job {
    let mut session = FlashSessionT::default();
    session.id = FLASH_SESSION;
    session.segments = Some(
        segments
            .iter()
            .map(|s| {
                let mut segment = FlashSegmentT::default();
                segment.artifact_offset = s.artifact_offset;
                segment.length = s.length;
                segment.block_size = s.block_size;
                segment
            })
            .collect(),
    );
    // UDS on ISO 15765 as `LinkConfig::iso15765(0x7E0, 0x7E8)` opens it: a document's job runs
    // on the link its variant declares (ADR-324).
    let mut protocol = ProtocolT::default();
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = 0x7E0;
    protocol.rx_id = 0x7E8;
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some("sim".into());
    variant.protocols = Some(vec![protocol]);
    variant.flash_sessions = Some(vec![session]);
    // The identity fields: F187 and F189, as long as the ECU's values.
    variant.services = Some(vec![
        read_service(1, 0xF187, PART_NUMBER.len()),
        read_service(2, 0xF189, SW_VERSION.len()),
    ]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    document.procedure =
        Some(postcard::to_allocvec(&download_program(segments)).expect("program should serialize"));
    let document = JobDocument::new(encode_ecu_document(&document)).expect("the document loads");
    let artifact = Artifact::read(artifact_path).expect("the artifact file reads");
    Job::from_document(document)
        .expect("the document carries its procedure")
        .with_artifact(*artifact.sha256(), artifact)
}

fn job_key() -> JobKey {
    job_key_named("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e70")
}

/// The second job's key, for a journal of its own.
fn long_block_job_key() -> JobKey {
    job_key_named("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e71")
}

fn job_key_named(id: &str) -> JobKey {
    JobKey {
        job_id: shared_proto::JobId(id.to_owned()),
        generation: 1,
    }
}

/// Removes the temporary journal directory however the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn agent_job_downloads_more_blocks_than_the_counter_holds() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let pid = std::process::id();
    let config_path = std::env::temp_dir().join(format!("agent-flash-{pid}-{nanos}-sim-vci.toml"));
    let ecu_path = std::env::temp_dir().join(format!("agent-flash-{pid}-{nanos}-ecu.json"));
    let _config = TempFile(config_path.clone());
    let _ecu = TempFile(ecu_path.clone());
    let journal_dir = std::env::temp_dir().join(format!("agent-flash-{pid}-{nanos}-journal"));
    std::fs::create_dir_all(&journal_dir).expect("journal directory");
    let _journal = TempDir(journal_dir.clone());
    let long_block_journal_dir =
        std::env::temp_dir().join(format!("agent-flash-{pid}-{nanos}-long-block-journal"));
    std::fs::create_dir_all(&long_block_journal_dir).expect("journal directory");
    let _long_block_journal = TempDir(long_block_journal_dir.clone());
    // The artifact files: the first job's segments end at the end of the file, the second
    // job's single block is the whole file.
    let artifact_dir = std::env::temp_dir().join(format!("agent-flash-{pid}-{nanos}-artifacts"));
    std::fs::create_dir_all(&artifact_dir).expect("artifact directory");
    let _artifacts = TempDir(artifact_dir.clone());
    let accepted_len = (u64::from(BLOCKS) + LONGEST_BLOCK as u64) as usize;
    std::fs::write(
        artifact_dir.join("accepted.bin"),
        artifact_bytes(accepted_len),
    )
    .expect("artifact file should be writable");
    std::fs::write(
        artifact_dir.join("long-block.bin"),
        artifact_bytes(LONGEST_BLOCK + 1),
    )
    .expect("artifact file should be writable");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    let definition_dir =
        std::env::temp_dir().join(format!("agent-flash-{pid}-{nanos}-definitions"));
    std::fs::create_dir_all(&definition_dir).expect("definition directory");
    let _definitions = TempDir(definition_dir.clone());
    j2534_0404_mock::write_definition(&definition_dir, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    // No security access, so the program needs no seed-key computation.
    std::fs::write(
        &ecu_path,
        r#"{"vin": "NGRSIMECU00000003", "part_number": "NGR-SIM-ECU", "sw_version": "1.0.0",
            "response_delay_ms": 0, "drop_at_block": null, "require_security_access": false,
            "require_gateway_auth": false, "fail_checksum": false}"#,
    )
    .expect("ECU config file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definition_dir);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::set_var("NGR_SIM_ECU_CONFIG", &ecu_path);
        std::env::remove_var("NGR_SIM_ECU_STATE");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // A wedged service must fail the test, not hang the CI job.
    runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(120),
                run_the_jobs(
                    journal_dir.clone(),
                    long_block_journal_dir.clone(),
                    artifact_dir.clone(),
                ),
            )
            .await
        })
        .expect("the flow should finish in time");

    let journal = Journal::read(&journal_dir, &job_key()).expect("the job's journal");
    let facts = &journal.facts;
    assert_eq!(facts.ecu_hardware_part_number.as_deref(), Some(PART_NUMBER));
    assert_eq!(
        facts.pre_erase_software_version.as_deref(),
        Some(SW_VERSION)
    );
    let transfer = facts.transfer.as_ref().expect("a transfer-start marker");
    assert_eq!(transfer.started_at.pc, 3);
    assert_eq!(transfer.last_block, Some(BLOCKS + 1));
    let exit = transfer
        .exit
        .as_ref()
        .expect("a RequestTransferExit marker");
    assert!(exit.complete);

    assert_comm_log(&journal_dir);

    // The refused block was never sent, so the second job's journal records no block.
    let journal =
        Journal::read(&long_block_journal_dir, &long_block_job_key()).expect("the second journal");
    let transfer = journal
        .facts
        .transfer
        .as_ref()
        .expect("a transfer-start marker");
    assert_eq!(transfer.last_block, None);
    assert!(transfer.exit.is_none());
}

/// The first job's communication log: `vci_connected` before `link_opened`, with the J2534
/// versions `sim-vci` reports and the worker's width, then the part number and the software
/// version as `identity` events, in the order they were read.
fn assert_comm_log(journal_dir: &std::path::Path) {
    let path = std::fs::read_dir(journal_dir)
        .expect("the journal directory")
        .map(|entry| entry.expect("an entry").path())
        .find(|path| path.to_string_lossy().ends_with(".commlog.jsonl"))
        .expect("a communication log beside the journal");
    let text = std::fs::read_to_string(&path).expect("the log");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    let events: Vec<&str> = lines
        .iter()
        .map(|line| line["event"].as_str().expect("an event"))
        .collect();
    assert_eq!(
        &events[..4],
        ["header", "job_start", "vci_connected", "link_opened"],
        "{text}"
    );
    let connected = &lines[2];
    assert_eq!(connected["api"], "j2534_0404", "{connected}");
    assert_eq!(connected["unsigned_long_bytes"], long_size(), "{connected}");
    let version = &connected["version"];
    // `sim-vci`'s `FIRMWARE_VERSION` and `DLL_VERSION` (`crates/sim-vci/src/lib.rs`).
    assert_eq!(version["firmware_version"], "NGR-SIM 1.0", "{connected}");
    assert!(
        version["library_version"]
            .as_str()
            .is_some_and(|library| library.starts_with("sim-vci ")),
        "{connected}"
    );
    assert!(version["api_version"].is_u64(), "{connected}");
    assert!(version.get("vendor").is_none(), "{connected}");
    assert_eq!(connected["drift_bound"]["source"], "default", "{connected}");
    let identities: Vec<(&str, &str)> = lines
        .iter()
        .filter(|line| line["event"] == "identity")
        .map(|line| {
            assert_eq!(line["outcome"], "value", "{line}");
            (
                line["kind"].as_str().expect("a kind"),
                line["text"].as_str().expect("a text value"),
            )
        })
        .collect();
    assert_eq!(
        identities,
        [
            (
                "hardware_part_number",
                std::str::from_utf8(PART_NUMBER).unwrap()
            ),
            ("software_version", std::str::from_utf8(SW_VERSION).unwrap()),
        ],
        "{text}"
    );
}

/// Guards for one job, in a lock directory of this call's own: the slot is device-wide, so
/// jobs of parallel tests must not share a directory. `writes` takes the reprogramming slot.
fn job_guards(writes: bool) -> JobGuards {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let setup = GuardSetup {
        dir: std::env::temp_dir().join(format!(
            "agent-flash-locks-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        vci: "agent-flash-vci".to_owned(),
    };
    let never = AtomicBool::new(false);
    let poll = Duration::from_millis(10);
    if writes {
        JobGuards::take(&setup, poll, &never)
    } else {
        JobGuards::take_vci_only(&setup, poll, &never)
    }
    .expect("guards should be free")
}

async fn run_the_jobs(
    journal_dir: PathBuf,
    long_block_journal_dir: PathBuf,
    artifact_dir: PathBuf,
) {
    let worker = WorkerProcess::launch(
        std::path::Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        OperatingMode::User,
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            long_size: Some(long_size()),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");

    let worker_ref = &worker;
    let run = move |job, dir, key| async move {
        let JobRun {
            result: state,
            guards: _guards,
            ..
        } = run_program_journaled(
            worker_ref
                .connect(&ConnectOptions::default())
                .await
                .expect("client should connect"),
            &LinkConfig::iso15765(0x7E0, 0x7E8),
            Job::with_vci(
                job,
                VciFacts {
                    api: Api::J2534,
                    long_size: long_size(),
                },
            ),
            JobLimits::default(),
            JournalSetup {
                dir,
                key,
                sources: ServiceSources::default(),
                vin: None,
                intended_software_version: None,
            },
            job_guards(true),
        )
        .await;
        state
    };

    let state = run(
        download_job(&accepted_segments(), &artifact_dir.join("accepted.bin")),
        journal_dir,
        job_key(),
    )
    .await
    .expect("the download job should finish");
    // RequestTransferExit is positive only when every block, in counter order across the wrap
    // from 0xFF to 0x00, arrived, the one at the ECU's limit included.
    assert_eq!(
        state.stack.last(),
        Some(&Value::Bytes(vec![0x77])),
        "{:?}",
        state.stack.last()
    );

    // One byte over the limit. The ECU would refuse the block itself (NRC 0x13), which the
    // host would report as `TransferRefused`; `BlockTooLong` means it was never sent.
    let refused = run(
        download_job(&long_block_segments(), &artifact_dir.join("long-block.bin")),
        long_block_journal_dir,
        long_block_job_key(),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(JobError::Host {
                source: HostError::BlockTooLong {
                    block: 1,
                    length: 0x0103,
                    max: 0x0102,
                },
                ..
            })
        ),
        "{refused:?}"
    );

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}
