//! `hw_t2_t3_read` (design 13.5, ADR-275): on a bench ECU or a vehicle, the read parts of a CSV +
//! Starlark definition run through the agent's job runner, on the harness's guarded client
//! (ADR-334). The profile names the definition (`[read]`): its tables, and the Starlark file of
//! a read procedure. The check identifies the ECU's variant as a server does before it sends a
//! job (design 8.9.1: each candidate variant's identification services are sent to the ECU, and
//! the variant whose patterns the answers match is selected), then runs that variant's read
//! procedure, which checks the values it decodes and fails on one it does not expect.
//!
//! Every job runs through the guard, so on T2 and T3 a write, routine, reset or security access
//! is refused before it reaches the worker: the check declares no operation beyond reads. A job
//! whose program writes is refused before it starts.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use agent::guards::{GuardSetup, JobGuards};
use agent::{Job, JobDocument, JobLimits, JobRun, run_program};
use diag_ir::VmState;
use diag_ir::declaration::load_ecu_document;
use diag_ir::identify::{identification_document, identified};

use crate::guard::GuardedClient;
use crate::profile::ReadDefinition;

/// The candidate variants of a definition: each variant's short name and its document, with the
/// read procedure as the document's procedure.
pub type Documents = Vec<(String, Vec<u8>)>;

/// Why the check did not pass.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// The profile or the definition is not what the check needs; nothing was sent.
    #[error("{0}")]
    Refused(String),
    /// A job did not finish: the guard refused a request, the ECU did not answer, or the
    /// procedure failed on a value it did not expect.
    #[error("the {job} job failed: {error}")]
    Job { job: String, error: String },
    /// No candidate variant's patterns match the ECU's answers.
    #[error("the ECU's answers match none of the definition's variants")]
    NotIdentified,
}

/// What the check found: the variant identified and the read procedure's final state.
#[derive(Debug)]
pub struct ReadOutcome {
    pub variant: String,
    pub state: VmState,
}

/// Converts the profile's definition: every file of `read.definition` but the Starlark ones,
/// and the read procedure `read.procedure`, into one document per variant (design 8.4). A file
/// is a Starlark one by its `.star` extension in any case, as the frontend reads it, so no other
/// procedure in the directory is converted beside the one the profile names.
pub fn convert(read: &ReadDefinition) -> Result<Documents, CheckError> {
    let refused = |what: String| CheckError::Refused(format!("the read definition: {what}"));
    if !is_starlark(&read.procedure) {
        return Err(refused(format!(
            "the procedure {:?} is not a Starlark (`.star`) file",
            read.procedure
        )));
    }
    let entries = std::fs::read_dir(&read.definition)
        .map_err(|e| refused(format!("{}: {e}", read.definition.display())))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| refused(e.to_string()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = file_name(&path)?;
        if is_starlark(&name) {
            continue;
        }
        files.push((name, read_text(&path)?));
    }
    let procedure = read.definition.join(&read.procedure);
    files.push((read.procedure.clone(), read_text(&procedure)?));
    files.sort();
    let tables = diag_frontend::tables::read_files(files)
        .map_err(|e| refused(format!("the tables do not read: {e:?}")))?;
    let definition = diag_frontend::tables::resolve(tables)
        .map_err(|e| refused(format!("the tables do not resolve: {e:?}")))?;
    let procedures = diag_frontend::convert::lower_procedures(&definition)
        .map_err(|e| refused(format!("the procedure does not lower: {e:?}")))?;
    let mut documents = Vec::new();
    for document in diag_frontend::convert::to_ir(&definition, &procedures).documents {
        let name = load_ecu_document(&document.bytes)
            .ok()
            .and_then(|loaded| loaded.variant())
            .and_then(|variant| variant.short_name().map(str::to_owned))
            .ok_or_else(|| refused("a document without a named variant".to_owned()))?;
        documents.push((name, document.bytes));
    }
    if documents.is_empty() {
        return Err(refused("no variant".to_owned()));
    }
    Ok(documents)
}

/// Whether the frontend reads `name` as a procedure file (`diag_frontend::tables::read_files`).
fn is_starlark(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".star")
}

fn file_name(path: &Path) -> Result<String, CheckError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| CheckError::Refused(format!("{}: not a UTF-8 name", path.display())))
}

/// A file's text, with CRLF line ends made LF, as the frontend reads them.
fn read_text(path: &Path) -> Result<Vec<u8>, CheckError> {
    std::fs::read_to_string(path)
        .map(|text| text.replace("\r\n", "\n").into_bytes())
        .map_err(|e| CheckError::Refused(format!("{}: {e}", path.display())))
}

/// Identifies the ECU's variant among `documents` and runs its read procedure, each job on
/// `client` with the link its document declares. `work` holds the jobs' vehicle locks and their
/// communication logs, which the procedures' `diag.log` messages go to (ADR-348 item 6).
pub async fn run(
    client: &GuardedClient,
    documents: &[(String, Vec<u8>)],
    work: &Path,
) -> Result<ReadOutcome, CheckError> {
    let mut runs = Vec::new();
    for (name, document) in documents {
        let (identification, services) = identification_document(document).map_err(|e| {
            CheckError::Refused(format!("{name}'s identification run does not build: {e}"))
        })?;
        let state = run_job(
            client,
            &format!("{name} identification"),
            identification,
            work,
        )
        .await?;
        runs.push((services, state));
    }
    let bytes: Vec<&[u8]> = documents
        .iter()
        .map(|(_, bytes)| bytes.as_slice())
        .collect();
    let variant = identified(&bytes, &runs)
        .map_err(|e| CheckError::Refused(format!("the identification answers: {e}")))?
        .ok_or(CheckError::NotIdentified)?;
    let (name, document) = &documents[variant];
    let state = run_job(client, &format!("{name} read"), document.clone(), work).await?;
    Ok(ReadOutcome {
        variant: name.clone(),
        state,
    })
}

/// Runs `document`'s procedure as one job on `client` and returns its final state.
async fn run_job(
    client: &GuardedClient,
    job_name: &str,
    document: Vec<u8>,
    work: &Path,
) -> Result<VmState, CheckError> {
    let failed = |error: String| CheckError::Job {
        job: job_name.to_owned(),
        error,
    };
    let document = JobDocument::new(document).map_err(|e| failed(e.to_string()))?;
    let job = Job::from_document(document)
        .map_err(|e| failed(e.to_string()))?
        .with_comm_log_dir(work.join("commlog"));
    if job.writes() {
        return Err(CheckError::Refused(format!(
            "the {job_name} job writes, and the check runs reads only"
        )));
    }
    let link = job.declared_link().map_err(|e| failed(e.to_string()))?;
    let guards = job_guards(&work.join("locks")).map_err(|e| failed(e.to_string()))?;
    let JobRun {
        result,
        guards: _guards,
        ..
    } = run_program(client.clone(), &link, job, JobLimits::default(), guards).await;
    result.map_err(|e| failed(e.to_string()))
}

/// The guards of a read job: the VCI's slot only (design 8.2.5).
fn job_guards(dir: &Path) -> Result<JobGuards, agent::guards::GuardError> {
    let setup = GuardSetup {
        dir: PathBuf::from(dir),
        vci: "hw-read-vci".to_owned(),
    };
    JobGuards::take_vci_only(&setup, Duration::from_millis(10), &AtomicBool::new(false))
}

#[cfg(test)]
#[path = "t2_t3_read_tests.rs"]
mod tests;
