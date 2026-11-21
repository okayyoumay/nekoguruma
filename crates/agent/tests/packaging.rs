//! The shipped Linux packaging files (`packaging/`) agree with each other and with the agent's
//! built-in device state root (design 6.2, ADR-460). Static: no systemd is needed, and the test
//! runs on every platform.

use std::path::{Path, PathBuf};

fn packaging(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The `key=value` lines of the unit's `section`, in order (comments and blank lines dropped).
fn section(unit: &str, section: &str) -> Vec<(String, String)> {
    let mut current = String::new();
    let mut found = Vec::new();
    for line in unit.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name.to_owned();
        } else if current == section
            && let Some((key, value)) = line.split_once('=')
        {
            found.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    found
}

/// The users the sysusers file creates (`u` lines, second field).
fn sysusers_users(conf: &str) -> Vec<String> {
    conf.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("u")).then(|| fields.next().unwrap_or_default().to_owned())
        })
        .collect()
}

#[test]
fn the_unit_and_the_sysusers_file_match_the_agents_device_root() {
    let unit = packaging("systemd/ngr-agent.service");
    let service = section(&unit, "Service");
    let values = |key: &str| -> Vec<&str> {
        service
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    };
    let single = |key: &str| -> Option<&str> {
        match values(key).as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    };
    let mut problems = Vec::new();

    let user = single("User");
    match user {
        None => problems.push("User= is not set exactly once".to_owned()),
        Some("root" | "0") => problems.push("User= is root".to_owned()),
        Some(_) => {}
    }
    if let Some(user) = user {
        let users = sysusers_users(&packaging("sysusers.d/ngr-agent.conf"));
        if users != [user] {
            problems.push(format!("sysusers creates {users:?}, but User= is {user:?}"));
        }
    }
    if !values("DynamicUser").is_empty() {
        problems.push("DynamicUser= is set; the account must be the static one".to_owned());
    }

    match single("StateDirectory") {
        None => problems.push("StateDirectory= is not set exactly once".to_owned()),
        Some(dir) => {
            let root = PathBuf::from("/var/lib").join(dir);
            if root != Path::new(agent::state::DEVICE_ROOT) {
                problems.push(format!(
                    "/var/lib/{dir} is not the agent's device root {}",
                    agent::state::DEVICE_ROOT
                ));
            }
        }
    }
    if single("StateDirectoryMode") != Some("0700") {
        problems.push(format!(
            "StateDirectoryMode= is {:?}, not 0700",
            values("StateDirectoryMode")
        ));
    }

    match single("ExecStart") {
        None => problems.push("ExecStart= is not set exactly once".to_owned()),
        Some(command) => {
            let words: Vec<&str> = command.split_whitespace().collect();
            let program = words.first().copied().unwrap_or_default();
            if Path::new(program).file_name().and_then(|n| n.to_str()) != Some("ngr-agent") {
                problems.push(format!("ExecStart= runs {program:?}, not ngr-agent"));
            }
            if words.get(1..) != Some(&["connect", "--mode", "device"][..]) {
                problems.push(format!(
                    "ExecStart= arguments are {:?}, not connect --mode device",
                    words.get(1..).unwrap_or_default()
                ));
            }
        }
    }

    // The agent key (ADR-498): one encrypted credential, named and placed as the agent's
    // constants say, and no other way to hand a credential to the unit.
    let expected = format!(
        "{}:{}",
        agent::identity::DEVICE_CREDENTIAL_NAME,
        agent::identity::DEVICE_CREDENTIAL_PATH
    );
    match values("LoadCredentialEncrypted").as_slice() {
        [one] if *one == expected => {}
        other => problems.push(format!(
            "LoadCredentialEncrypted= is {other:?}, not exactly [{expected:?}]"
        )),
    }
    for key in ["LoadCredential", "SetCredential", "SetCredentialEncrypted"] {
        if !values(key).is_empty() {
            problems.push(format!(
                "{key}= is set; only LoadCredentialEncrypted= may be"
            ));
        }
    }

    assert!(problems.is_empty(), "packaging mismatches: {problems:#?}");
}

#[test]
fn the_credential_path_is_the_store_directory_and_the_name() {
    assert_eq!(
        agent::identity::DEVICE_CREDENTIAL_PATH,
        format!(
            "{}/{}",
            agent::identity::DEVICE_CREDSTORE_DIR,
            agent::identity::DEVICE_CREDENTIAL_NAME
        )
    );
    assert_eq!(
        agent::identity::DEVICE_CREDENTIAL_NAME,
        agent::identity::store::SecretName::agent_key(worker_host::service::OperatingMode::Device)
            .as_str()
    );
}
