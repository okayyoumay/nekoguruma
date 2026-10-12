use std::{
    collections::HashMap,
    io::{self, ErrorKind},
};

use url::Url;
use vci_service_launcher::mode::{MODE_QUERY_KEY, OperatingMode};

#[derive(Debug)]
pub struct StartupUri {
    pub library_name: String,
    pub options: HashMap<String, String>,
}

pub fn parse_startup_uri(uri: &str) -> Result<StartupUri, Box<dyn std::error::Error>> {
    let parsed = Url::parse(uri)?;
    if parsed.scheme() != "iso22900" {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "first argument must use scheme 'iso22900'",
        )
        .into());
    }

    let library_name = parsed.path().trim().to_string();
    if library_name.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "first argument must include a library name after 'iso22900:'",
        )
        .into());
    }

    let options = parsed.query_pairs().into_owned().collect::<HashMap<_, _>>();

    Ok(StartupUri {
        library_name,
        options,
    })
}

/// Service startup configuration derived from command-line arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupConfig {
    pub(crate) library_name: String,
    pub(crate) requested_port: Option<u16>,
    /// The device's operating mode, passed by the agent (ADR-281 Decision item 2). Design 7.2
    /// gives it its meaning for a failed library check; library loading does not consult it.
    pub(crate) mode: OperatingMode,
}

impl StartupConfig {
    /// The operating mode this worker was started in.
    pub fn mode(&self) -> OperatingMode {
        self.mode
    }
}

/// Parse the second command-line argument as `iso22900[+arch]:<library>?mode=<device|user>[&port=<u16>]`.
pub fn parse_startup_config(
    args: impl IntoIterator<Item = impl Into<String>>,
) -> Result<StartupConfig, io::Error> {
    let arg = args
        .into_iter()
        .nth(1)
        .ok_or_else(|| invalid_input("missing startup argument"))?
        .into();
    parse_startup_arg(arg.as_str())
}

pub(crate) fn parse_startup_arg(startup_arg: &str) -> Result<StartupConfig, io::Error> {
    let startup_uri = Url::parse(startup_arg)
        .map_err(|e| invalid_input(format!("invalid startup argument: {e}")))?;

    let uri_schemes = startup_uri.scheme().split('+').collect::<Vec<_>>();
    if uri_schemes.first() != Some(&"iso22900") {
        return Err(invalid_input(
            "startup argument must use format 'iso22900:<library name>?mode=<device|user>&port=<grpc port number>&...'",
        ));
    }

    #[cfg(windows)]
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(arch_in_schema) = uri_schemes.get(1) {
            let native = iso22900_registry::LibraryArch::get_native();
            let Some(native_str): Option<&str> = native.into() else {
                return Err(invalid_input(format!(
                    "URI scheme architecture {arch_in_schema} does not match with native architecture"
                )));
            };
            if *arch_in_schema != native_str {
                return Err(invalid_input(format!(
                    "URI scheme architecture {arch_in_schema} does not match with native architecture"
                )));
            }
        }
    }

    let library_name = startup_uri.path().trim();
    if library_name.is_empty() {
        return Err(invalid_input(
            "startup argument must include a non-empty library name",
        ));
    }

    let mut requested_port = None;
    let mut mode = None;
    for (key, value) in startup_uri.query_pairs() {
        if key == MODE_QUERY_KEY {
            mode = Some(parse_mode(mode, &value)?);
            continue;
        }
        if key != "port" {
            continue;
        }
        if requested_port.is_some() {
            return Err(invalid_input(
                "startup argument must not repeat the port query parameter",
            ));
        }
        requested_port = Some(
            value
                .parse::<u16>()
                .map_err(|_| invalid_input("port query parameter must be a valid u16"))?,
        );
    }

    let mode = mode.ok_or_else(|| {
        invalid_input(
            "startup argument must include the operating mode ('mode=device' or 'mode=user'); \
             the worker does not infer it",
        )
    })?;

    Ok(StartupConfig {
        mode,
        library_name: library_name.to_string(),
        requested_port,
    })
}

/// Parses one `mode` query value; a repeated `mode` is refused even when both values agree.
fn parse_mode(previous: Option<OperatingMode>, value: &str) -> Result<OperatingMode, io::Error> {
    if previous.is_some() {
        return Err(invalid_input(
            "startup argument must not repeat the mode query parameter",
        ));
    }
    value
        .parse()
        .map_err(|e| invalid_input(format!("invalid startup argument: {e}")))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::{parse_startup_arg, parse_startup_uri};
    use vci_service_launcher::mode::OperatingMode;

    #[test]
    fn parses_library_and_options() {
        let startup_uri =
            parse_startup_uri("iso22900:my-lib?subscriber=http://127.0.0.1:9000&id=req-123&x=1")
                .expect("startup URI should parse");

        assert_eq!(startup_uri.library_name, "my-lib");
        assert_eq!(
            startup_uri.options.get("subscriber").map(String::as_str),
            Some("http://127.0.0.1:9000")
        );
        assert_eq!(
            startup_uri.options.get("id").map(String::as_str),
            Some("req-123")
        );
        assert_eq!(startup_uri.options.get("x").map(String::as_str), Some("1"));
    }

    #[test]
    fn rejects_wrong_scheme() {
        let error = parse_startup_uri("http://example.com").expect_err("wrong scheme should fail");
        assert!(
            error
                .to_string()
                .contains("first argument must use scheme 'iso22900'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_missing_library_name() {
        let error = parse_startup_uri("iso22900:").expect_err("missing library should fail");
        assert!(
            error
                .to_string()
                .contains("first argument must include a library name"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_startup_config_extracts_library_and_port() {
        let config =
            parse_startup_arg("iso22900:my-lib?port=50051&mode=user").expect("should parse");
        assert_eq!(config.library_name, "my-lib");
        assert_eq!(config.requested_port, Some(50051));
    }

    #[test]
    fn parse_startup_config_no_port() {
        let config =
            parse_startup_arg("iso22900:my-lib?mode=user").expect("should parse without port");
        assert_eq!(config.library_name, "my-lib");
        assert_eq!(config.requested_port, None);
    }

    #[test]
    fn parse_startup_config_rejects_wrong_scheme() {
        let error = parse_startup_arg("http://example.com").expect_err("wrong scheme");
        assert!(
            error
                .to_string()
                .contains("startup argument must use format")
        );
    }

    #[test]
    fn parse_startup_config_rejects_duplicate_port() {
        let error = parse_startup_arg("iso22900:lib?mode=user&port=1234&port=5678")
            .expect_err("duplicate port");
        assert!(error.to_string().contains("must not repeat"));
    }

    #[test]
    fn parse_startup_config_keeps_the_mode() {
        for (value, mode) in [
            ("device", OperatingMode::Device),
            ("user", OperatingMode::User),
        ] {
            let config = parse_startup_arg(&format!("iso22900:demo-lib?port=1&mode={value}"))
                .expect("a known mode should parse");
            assert_eq!(config.mode(), mode);
            assert_eq!(config.requested_port, Some(1));
        }
    }

    #[test]
    fn parse_startup_config_rejects_a_missing_mode() {
        let err = parse_startup_arg("iso22900:demo-lib?port=1").expect_err("no mode should fail");
        assert!(
            err.to_string().contains("must include the operating mode"),
            "{err}"
        );
    }

    #[test]
    fn parse_startup_config_rejects_an_unknown_mode() {
        for query in ["mode=", "mode=Device", "mode=service"] {
            let err = parse_startup_arg(&format!("iso22900:demo-lib?{query}"))
                .expect_err("an unknown mode should fail");
            assert!(
                err.to_string().contains("unknown operating mode"),
                "{query}: {err}"
            );
        }
    }

    #[test]
    fn parse_startup_config_rejects_a_repeated_mode() {
        for query in ["mode=user&mode=user", "mode=device&mode=user"] {
            let err = parse_startup_arg(&format!("iso22900:demo-lib?{query}"))
                .expect_err("a repeated mode should fail");
            assert!(
                err.to_string().contains("must not repeat the mode"),
                "{query}: {err}"
            );
        }
    }
}
