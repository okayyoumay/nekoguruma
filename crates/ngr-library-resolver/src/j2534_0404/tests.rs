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

#[test]
fn reject_wrong_type_keys() {
    for bad in ["LongSize = \"8\"\n", "LongSize = true\n", "CAN = \"1\"\n"] {
        let text = format!("{}{bad}", minimal("X"));
        assert!(
            matches!(Definition::parse(&text), Err(DefinitionError::Syntax(_))),
            "{bad}"
        );
    }
}

#[test]
fn reject_whitespace_names() {
    assert!(matches!(
        Definition::parse(&minimal("   ")),
        Err(DefinitionError::EmptyName)
    ));
    for name in [" X", "X ", "X\\t"] {
        assert!(
            matches!(
                Definition::parse(&minimal(name)),
                Err(DefinitionError::NameWhitespace)
            ),
            "{name:?}"
        );
    }
}

#[test]
fn reject_bad_library_text() {
    let nul = "Name = \"X\"\nFunctionLibrary = \"/a\\u0000b\"\n";
    assert!(matches!(
        Definition::parse(nul),
        Err(DefinitionError::NulInPath {
            key: "FunctionLibrary"
        })
    ));
    let ws = format!(
        "Name = \"X\"\nFunctionLibrary = \" {}\"\n",
        abs("x").display()
    );
    assert!(ws.contains("= \" "));
    assert!(matches!(
        Definition::parse(&ws.replace('\\', "/")),
        Err(DefinitionError::PathWhitespace { .. })
    ));
    let dotdot = format!(
        "Name = \"X\"\nFunctionLibrary = {}\n",
        lit(&abs("a").join("..").join("x.so"))
    );
    assert!(matches!(
        Definition::parse(&dotdot),
        Err(DefinitionError::ParentDirInPath { .. })
    ));
}

#[test]
fn reject_bad_search_path_text() {
    let nul = format!("{}SearchPaths = [\"/a\\u0000\"]\n", minimal("X"));
    assert!(matches!(
        Definition::parse(&nul),
        Err(DefinitionError::NulInPath { key: "SearchPaths" })
    ));
    let ws = format!("{}SearchPaths = [\"/a \"]\n", minimal("X"));
    assert!(matches!(
        Definition::parse(&ws),
        Err(DefinitionError::PathWhitespace { .. })
    ));
    let dotdot = format!(
        "{}SearchPaths = [{}]\n",
        minimal("X"),
        lit(&abs("a").join(".."))
    );
    assert!(matches!(
        Definition::parse(&dotdot),
        Err(DefinitionError::ParentDirInPath { .. })
    ));
}

#[test]
fn oversized_file_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let mut text = minimal("Big");
    text.push_str(&format!("# {}\n", "x".repeat(MAX_DEFINITION_SIZE as usize)));
    fs::write(dir.path().join("big.toml"), text).unwrap();
    let defs = read_definitions(dir.path()).unwrap();
    assert!(defs.valid.is_empty());
    assert!(matches!(defs.invalid[0].1, DefinitionError::TooLarge));
}

#[test]
fn directory_named_toml_is_invalid_and_bare_dot_toml_ignored() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("d.toml")).unwrap();
    fs::write(dir.path().join(".toml"), minimal("Bare")).unwrap();
    fs::write(dir.path().join(".hidden.toml"), minimal("Hidden")).unwrap();
    let defs = read_definitions(dir.path()).unwrap();
    assert_eq!(defs.valid.len(), 1);
    assert_eq!(defs.valid[0].1.name, "Hidden");
    assert_eq!(defs.invalid.len(), 1);
    assert!(matches!(
        defs.invalid[0].1,
        DefinitionError::NotARegularFile
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_toml_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real.txt");
    fs::write(&target, minimal("Linked")).unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("link.toml")).unwrap();
    let defs = read_definitions(dir.path()).unwrap();
    assert!(defs.valid.is_empty());
    assert!(matches!(
        defs.invalid[0].1,
        DefinitionError::NotARegularFile
    ));
    assert!(matches!(
        resolve_in_dir(dir.path(), "Linked"),
        Err(ResolveError::NotFound {
            skipped_invalid: 1,
            ..
        })
    ));
}

/// A definition directory that is itself a symlink is refused, even when the files it points to
/// are valid definitions.
#[cfg(unix)]
#[test]
fn a_linked_definition_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("a.toml"), minimal("Linked")).unwrap();
    let link = dir.path().join("j2534");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(matches!(
        read_definitions(&link),
        Err(ResolveError::LinkedDirectory { .. })
    ));
    assert!(matches!(
        resolve_in_dir(&link, "Linked"),
        Err(ResolveError::LinkedDirectory { .. })
    ));
    // The directory it points to is read as usual.
    assert!(resolve_in_dir(&real, "Linked").is_ok());
}

#[test]
fn results_are_sorted_by_path() {
    let dir = tempfile::tempdir().unwrap();
    for f in ["m", "z", "b", "q", "a", "x"] {
        fs::write(dir.path().join(format!("{f}.toml")), minimal("Same")).unwrap();
    }
    fs::write(dir.path().join("0bad.toml"), "junk").unwrap();
    let defs = read_definitions(dir.path()).unwrap();
    let paths: Vec<_> = defs.valid.iter().map(|(p, _)| p.clone()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    match resolve_in_dir(dir.path(), "Same") {
        Err(ResolveError::Ambiguous { files, .. }) => assert_eq!(files, sorted),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn not_found_message_mentions_skipped_only_when_nonzero() {
    let none = ResolveError::NotFound {
        name: "X".into(),
        skipped_invalid: 0,
    };
    assert_eq!(none.to_string(), "VCI 'X' not found");
    let some = ResolveError::NotFound {
        name: "X".into(),
        skipped_invalid: 2,
    };
    assert!(some.to_string().contains("2 invalid"));
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
    // `bad.toml` and the directory `sub.toml`.
    assert_eq!(defs.invalid.len(), 2);
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
            assert_eq!(skipped_invalid, 2);
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

/// A registry hit becomes a `Resolved` with its key name and library, from the registry.
#[cfg(windows)]
#[test]
fn a_registry_hit_maps_to_a_resolved() {
    use j2534_0404_registry::{J2534DeviceInfo, LibraryArch, LibrarySource};
    let library = std::env::temp_dir().join("vendor").join("j2534.dll");
    let resolved = resolved_from_registry(J2534DeviceInfo {
        device_name: "Vendor VCI".to_owned(),
        library_path: library.clone(),
        arch: LibraryArch::Native,
        source: LibrarySource::Registry,
    });
    assert_eq!(resolved.source, Source::Registry);
    assert_eq!(resolved.definition.name, "Vendor VCI");
    assert_eq!(resolved.definition.library, library);
    assert!(resolved.definition.protocols.is_empty());
    assert_eq!(resolved.definition.long_size, None);
    assert!(resolved.definition.search_paths.is_empty());
}

#[test]
fn naming_files_of_a_definition_and_of_the_registry() {
    let definition = Definition::parse(&minimal("x")).unwrap();
    let file = abs("x.toml");
    let from_file = Resolved {
        definition: definition.clone(),
        source: Source::Definition(file.clone()),
    };
    assert_eq!(from_file.naming_files(), [file]);
    let from_registry = Resolved {
        definition,
        source: Source::Registry,
    };
    assert!(from_registry.naming_files().is_empty());
}
