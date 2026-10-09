use std::fs;

use super::*;

/// An absolute path valid on the running platform.
fn abs(rel: &str) -> PathBuf {
    std::env::temp_dir().join("ngr-resolver-fixture").join(rel)
}

/// TOML literal string for a path (single quotes: no escaping, Windows-safe).
fn lit(p: &Path) -> String {
    format!("'{}'", p.display())
}

fn minimal(name: &str) -> String {
    format!(
        "Name = \"{name}\"\nFunctionLibrary = {}\n",
        lit(&abs("libx.so"))
    )
}

#[test]
fn parse_full_definition() {
    let text = format!(
        "Name = \"Acme - One\"\nVendor = \"Acme\"\nFunctionLibrary = {}\n\
         ConfigApplication = \"acme-config\"\nCAN = 1\nISO15765 = 1\nISO9141 = 0\n\
         LongSize = 8\nSearchPaths = [{}, {}]\n",
        lit(&abs("libacme.so")),
        lit(&abs("a")),
        lit(&abs("b")),
    );
    let d = Definition::parse(&text).unwrap();
    assert_eq!(d.name, "Acme - One");
    assert_eq!(d.vendor.as_deref(), Some("Acme"));
    assert_eq!(d.library, abs("libacme.so"));
    assert_eq!(d.config_application.as_deref(), Some("acme-config"));
    assert_eq!(d.protocols, ["CAN", "ISO15765"]);
    assert_eq!(d.long_size, Some(8));
    assert_eq!(d.search_paths, [abs("a"), abs("b")]);
}

#[test]
fn parse_minimal_definition() {
    let d = Definition::parse(&minimal("Min")).unwrap();
    assert_eq!(d.name, "Min");
    assert_eq!(d.vendor, None);
    assert!(d.protocols.is_empty());
    assert_eq!(d.long_size, None);
    assert!(d.search_paths.is_empty());
}

#[test]
fn reject_relative_library() {
    let e = Definition::parse("Name = \"X\"\nFunctionLibrary = \"lib/x.so\"\n").unwrap_err();
    assert!(matches!(e, DefinitionError::RelativeLibrary(_)), "{e}");
}

#[test]
fn reject_missing_required_keys() {
    let no_name = format!("FunctionLibrary = {}\n", lit(&abs("x")));
    assert!(matches!(
        Definition::parse(&no_name),
        Err(DefinitionError::Syntax(_))
    ));
    assert!(matches!(
        Definition::parse("Name = \"X\"\n"),
        Err(DefinitionError::Syntax(_))
    ));
}

#[test]
fn reject_empty_name() {
    assert!(matches!(
        Definition::parse(&minimal("")),
        Err(DefinitionError::EmptyName)
    ));
}

#[test]
fn reject_unknown_key() {
    let text = format!("{}Bogus = 1\n", minimal("X"));
    assert!(matches!(
        Definition::parse(&text),
        Err(DefinitionError::Syntax(_))
    ));
}

#[test]
fn reject_protocol_value_2() {
    let text = format!("{}CAN = 2\n", minimal("X"));
    assert!(matches!(
        Definition::parse(&text),
        Err(DefinitionError::InvalidProtocolValue {
            key: "CAN",
            value: 2
        })
    ));
}

#[test]
fn reject_long_size_6() {
    let text = format!("{}LongSize = 6\n", minimal("X"));
    assert!(matches!(
        Definition::parse(&text),
        Err(DefinitionError::InvalidLongSize(6))
    ));
}

#[test]
fn reject_relative_search_path() {
    let text = format!("{}SearchPaths = [\"rel/dir\"]\n", minimal("X"));
    assert!(matches!(
        Definition::parse(&text),
        Err(DefinitionError::RelativeSearchPath(_))
    ));
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    fs::write(p.join("one.toml"), minimal("One")).unwrap();
    fs::write(p.join("two.toml"), minimal("Two")).unwrap();
    fs::write(
        p.join("bad.toml"),
        "Name = \"Typo\"\nFunctionLibrary = \"rel.so\"\n",
    )
    .unwrap();
    fs::write(p.join("notes.txt"), minimal("Ignored")).unwrap();
    fs::write(p.join("UPPER.TOML"), minimal("Ignored")).unwrap();
    fs::create_dir(p.join("sub.toml")).unwrap();
    dir
}

#[test]
fn read_definitions_splits_valid_and_invalid() {
    let dir = fixture();
    let defs = read_definitions(dir.path()).unwrap();
    let names: Vec<_> = defs.valid.iter().map(|(_, d)| d.name.as_str()).collect();
    assert_eq!(names, ["One", "Two"]);
    assert_eq!(defs.invalid.len(), 1);
    assert!(defs.invalid[0].0.ends_with("bad.toml"));
}

#[test]
fn resolve_in_dir_by_exact_name() {
    let dir = fixture();
    let r = resolve_in_dir(dir.path(), "Two").unwrap();
    assert_eq!(r.definition.library, abs("libx.so"));
    assert_eq!(r.source, Source::Definition(dir.path().join("two.toml")));
    assert!(matches!(
        resolve_in_dir(dir.path(), "two"),
        Err(ResolveError::NotFound { .. })
    ));
}

#[test]
fn resolve_in_dir_not_found_counts_skipped_files() {
    let dir = fixture();
    match resolve_in_dir(dir.path(), "Typo") {
        Err(ResolveError::NotFound {
            name,
            skipped_invalid,
        }) => {
            assert_eq!(name, "Typo");
            assert_eq!(skipped_invalid, 1);
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn resolve_in_dir_ambiguous_name_is_refused() {
    let dir = fixture();
    fs::write(dir.path().join("one-again.toml"), minimal("One")).unwrap();
    match resolve_in_dir(dir.path(), "One") {
        Err(ResolveError::Ambiguous { name, files }) => {
            assert_eq!(name, "One");
            assert_eq!(files.len(), 2);
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn missing_directory_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent");
    let defs = read_definitions(&missing).unwrap();
    assert!(defs.valid.is_empty() && defs.invalid.is_empty());
    assert!(matches!(
        resolve_in_dir(&missing, "One"),
        Err(ResolveError::NotFound {
            skipped_invalid: 0,
            ..
        })
    ));
}

#[cfg(windows)]
#[test]
fn registry_unknown_name_is_not_found() {
    use j2534_0404_registry::RegistryViewMode;
    assert!(matches!(
        resolve_on_registry("__nonexistent_j2534_device__", RegistryViewMode::All),
        Err(ResolveError::NotFound { .. })
    ));
    assert!(matches!(
        resolve("__nonexistent_j2534_device__"),
        Err(ResolveError::NotFound { .. })
    ));
}
