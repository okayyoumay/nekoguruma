use std::{
    collections::HashMap,
    io::{self, ErrorKind},
};

use url::Url;

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
}

/// Parse the second command-line argument as `iso22900[+arch]:<library>?port=<u16>`.
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
            "startup argument must use format 'iso22900:<library name>?port=<grpc port number>&...'",
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
    for (key, value) in startup_uri.query_pairs() {
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

    Ok(StartupConfig {
        library_name: library_name.to_string(),
        requested_port,
    })
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::{parse_startup_arg, parse_startup_uri};

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
        let config = parse_startup_arg("iso22900:my-lib?port=50051").expect("should parse");
        assert_eq!(config.library_name, "my-lib");
        assert_eq!(config.requested_port, Some(50051));
    }

    #[test]
    fn parse_startup_config_no_port() {
        let config = parse_startup_arg("iso22900:my-lib").expect("should parse without port");
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
        let error =
            parse_startup_arg("iso22900:lib?port=1234&port=5678").expect_err("duplicate port");
        assert!(error.to_string().contains("must not repeat"));
    }
}
