use std::path::{Path, PathBuf};

use super::*;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../diag-frontend/examples")
}

/// The example ECU's tables and the end-to-end read procedure in one directory, as a profile's
/// `[read]` names them, with a routine procedure beside them that the check must leave out.
fn definition_dir(dir: &Path) -> ReadDefinition {
    for entry in std::fs::read_dir(examples().join("example-ecu")).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
    }
    for procedure in ["read.star", "routine.star"] {
        std::fs::copy(
            examples().join("end-to-end").join(procedure),
            dir.join(procedure),
        )
        .unwrap();
    }
    ReadDefinition {
        definition: dir.to_owned(),
        procedure: "read.star".to_owned(),
    }
}

/// The definition converts to one document per variant, each carrying the read procedure, and
/// the routine procedure in the same directory is not converted.
#[test]
fn the_definition_converts_to_a_document_per_variant_with_the_read_procedure() {
    let temp = tempfile::tempdir().unwrap();
    let read = definition_dir(temp.path());
    let documents = convert(&read).expect("the example converts");
    let names: Vec<&str> = documents.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["exampleEcuV1", "exampleEcuV2"]);
    for (name, bytes) in &documents {
        let document = JobDocument::new(bytes.clone()).expect("the document loads");
        let job = Job::from_document(document).expect("the document carries a procedure");
        assert!(!job.writes(), "{name}: the read procedure writes nothing");
        // Each identification run builds from the document.
        let (_, services) = identification_document(bytes).expect("the identification run");
        assert!(!services.is_empty(), "{name}");
    }
}

/// A definition the check cannot convert is refused before anything is sent.
#[test]
fn a_definition_that_does_not_convert_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let missing = ReadDefinition {
        definition: temp.path().join("absent"),
        procedure: "read.star".to_owned(),
    };
    assert!(matches!(convert(&missing), Err(CheckError::Refused(_))));
    let mut read = definition_dir(temp.path());
    read.procedure = "absent.star".to_owned();
    assert!(matches!(convert(&read), Err(CheckError::Refused(_))));
}
