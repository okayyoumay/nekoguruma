use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, ErrorKind};

use url::Url;

/// Service startup configuration derived from command-line arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupConfig {
    pub(crate) library_name: String,
    pub(crate) requested_port: Option<u16>,
}

/// One configured device-selection entry (ADR-107): a human-readable label
/// plus the connection-target string passed as `PassThruOpen`'s `pName`.
///
/// `pname` is `None` only for the single synthetic entry synthesized by
/// [`resolve_modules`] when the `modules` config key is absent entirely —
/// preserving pre-ADR-107 behavior byte-for-byte (`PassThruOpen(NULL, ...)`).
#[derive(Debug, Clone)]
pub(crate) struct ModuleEntry {
    pub(crate) label: String,
    pub(crate) pname: Option<CString>,
}

/// Resolves the `modules` array configured for `library_name`
/// (`config.apis.j2534-0404.libs."<lib>".modules`, see
/// `vci_service_config::find_modules`, ADR-107) into a non-empty
/// `Vec<ModuleEntry>`. Each entry's `pname` is converted to an owned
/// `CString` once here, at startup, so an embedded-NUL or non-ASCII `pname`
/// fails fast at startup instead of on every later
/// `PassThruOpen`/`ModuleConnect` call.
///
/// - `modules` absent entirely: returns a single synthetic entry
///   (`label = "j2534-0404"`, `pname = None`) matching pre-ADR-107 behavior.
/// - `modules = []` (present but empty): a startup error.
/// - Any entry's `pname` containing a non-ASCII byte or an embedded NUL byte:
///   a startup error — Windows J2534 DLLs read `pName` as an ANSI narrow
///   string, so an embedded NUL would silently truncate it and non-ASCII
///   bytes have no defined narrow-string encoding here.
/// - config file present but unreadable/unparseable: a startup error (does
///   not fall back to the default single module).
pub(crate) fn resolve_modules(
    arch: Option<&str>,
    library_name: &str,
) -> Result<Vec<ModuleEntry>, io::Error> {
    let configured = vci_service_launcher::config::find_modules("j2534-0404", arch, library_name)
        .map_err(|e| {
        invalid_input(format!(
            "cannot load the modules config for {library_name:?}: {e}; refusing to fall back \
                 to the default module (a malformed config file must not silently select a \
                 default device)"
        ))
    })?;
    build_module_entries(configured)
}

/// Pure validation/conversion core of [`resolve_modules`], factored out so
/// unit tests can exercise it directly against hand-built
/// `vci_service_config::ModuleConfigEntry` values instead of round-tripping
/// through a real config file.
fn build_module_entries(
    configured: Option<Vec<vci_service_launcher::config::ModuleConfigEntry>>,
) -> Result<Vec<ModuleEntry>, io::Error> {
    match configured {
        None => Ok(vec![ModuleEntry {
            label: "j2534-0404".to_string(),
            pname: None,
        }]),
        Some(entries) if entries.is_empty() => Err(invalid_input(
            "config.apis.j2534-0404....modules is present but empty; add at least one \
             [[...modules]] entry, or omit the key entirely for the default single-module \
             behavior",
        )),
        Some(entries) => entries
            .into_iter()
            .map(|entry| {
                if !entry.pname.is_ascii() {
                    return Err(invalid_input(format!(
                        "module {:?} pname {:?} must be ASCII (Windows J2534 DLLs read pName \
                         as an ANSI narrow string)",
                        entry.label, entry.pname
                    )));
                }
                let pname = CString::new(entry.pname.clone()).map_err(|_| {
                    invalid_input(format!(
                        "module {:?} pname {:?} must not contain an embedded NUL byte",
                        entry.label, entry.pname
                    ))
                })?;
                Ok(ModuleEntry {
                    label: entry.label,
                    pname: Some(pname),
                })
            })
            .collect(),
    }
}

/// Startup-rejection ceiling on a `shape = "raw"` entry's `input_bytes`/
/// `output_bytes` (design-advisor decision, PR #133 eleventh round). This is
/// NOT the removed fixed allocation cap from ADR-219's third round --
/// that substituted a smaller allocation for whatever the operator
/// configured, letting a native call proceed against an undersized
/// buffer (silent corruption). This ceiling never substitutes anything:
/// a value within it is honored exactly as configured (unchanged from
/// before this amendment); a value above it fails startup loudly instead
/// of letting `AlignedByteBuf::zeroed` attempt an unbounded allocation at
/// dispatch time, which Codex found could reach multi-`u32::MAX`-byte
/// sizes an operator typo (or a config file corruption) could produce
/// with no native DLL involvement at all -- a guaranteed-reproducible
/// process abort for every client, not the narrower "wrong-but-present
/// size causes native UB" residual already accepted for a value that
/// merely doesn't match the real native contract.
///
/// 16 MiB, chosen generously above any plausible real vendor buffer: the
/// largest buffer any SAE-defined J2534 object carries is
/// `PASSTHRU_MSG.Data[4128]` (`j2534-0404-sys/src/bindings/j2534_v0404.h`),
/// and this workspace's gRPC services accept tonic's default 4 MiB
/// inbound message limit (never overridden), so no client can even
/// deliver `input_bytes` worth of payload anywhere near this ceiling --
/// raising it is a one-line, reviewable change if a real vendor contract
/// ever legitimately needs more.
pub(crate) const VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES: u32 = 16 * 1024 * 1024;

/// A vendor IOCTL command's native contract, resolved once at startup from
/// `vci_service_config::VendorIoctlConfigEntry` (ADR-219, as amended). This
/// service's own dispatch (`rpc_misc.rs::rpc_io_ctl_vendor`) looks this up
/// by `cmd_id` before taking any lock -- the config table is the sole
/// source of a vendor IOCTL's real buffer shape/sizes, since the D-PDU/
/// J2534 API surfaces no self-describing metadata for a
/// tool-manufacturer-reserved command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VendorIoctlContract {
    /// `shape = "raw"`: `pInput`/`pOutput` point directly at raw byte
    /// buffers, exactly `input_bytes`/`output_bytes` long. `0` means that
    /// direction takes no buffer at all (native pointer stays `NULL`).
    Raw { input_bytes: u32, output_bytes: u32 },
    /// `shape = "sbyte_array"`: `pInput`/`pOutput` each point at a native
    /// `SBYTE_ARRAY`. No configured size is needed -- wrapped mode's own
    /// `SBYTE_ARRAY.NumOfBytes` mechanism self-describes the real written
    /// length, and the request's own `output_capacity` already bounds the
    /// allocation (capped at `rpc_misc::VENDOR_IOCTL_MAX_WRAPPED_CAPACITY`).
    /// `input_required`/`output_required` declare whether this command's
    /// native contract always expects a non-NULL `SBYTE_ARRAY` in that
    /// direction -- the wrapped-mode counterpart to `Raw`'s
    /// `input_bytes > 0`/`output_bytes > 0` implying the same requirement
    /// (Codex review, PR #133, design-advisor decision): unlike `Raw`,
    /// wrapped mode has no byte count to double as a presence requirement,
    /// since `SBYTE_ARRAY` self-describes length instead.
    SbyteArray {
        input_required: bool,
        output_required: bool,
    },
}

/// Parses one `vendor_ioctls` table entry: `key` is the `"0x<hex digits>"`
/// TOML key (the `cmd_id`), `entry` is its deserialized value. Returns the
/// decoded `(cmd_id, VendorIoctlContract)` pair, or a startup error naming
/// the offending key/shape so a typo surfaces at startup rather than as a
/// silent `FAILED_PRECONDITION` the first time a client uses that command.
fn parse_vendor_ioctl_entry(
    key: &str,
    entry: vci_service_launcher::config::VendorIoctlConfigEntry,
) -> Result<(u32, VendorIoctlContract), io::Error> {
    // Grammar is deliberately canonical (exactly 8 lowercase hex digits),
    // not merely "valid hex" -- without this, distinct TOML keys like
    // "0x00010001" and "0x10001" would decode to the same cmd_id and
    // silently collapse into one HashMap entry, with whichever alias's
    // contract happens to be inserted last (a source-map iteration order
    // this service does not control) winning over the other. That would
    // defeat this table's entire fail-fast purpose: a config author could
    // have two aliases of the same command with different shapes/sizes and
    // never find out which one is actually in effect. Mirrors the same
    // canonical-grammar rule ADR-218's `type_url` (`"pdu-cpst:0x<8
    // lowercase hex digits>"`) already applies for the identical reason.
    let hex = key.strip_prefix("0x").ok_or_else(|| {
        invalid_input(format!(
            "config.apis.j2534-0404....vendor_ioctls key {key:?} must be formatted \
             \"0x<8 lowercase hex digits>\" (the cmd_id in hex)"
        ))
    })?;
    if hex.len() != 8
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid_input(format!(
            "config.apis.j2534-0404....vendor_ioctls key {key:?} must be formatted \
             \"0x<8 lowercase hex digits>\" (the cmd_id in hex); got {hex:?}"
        )));
    }
    let cmd_id = u32::from_str_radix(hex, 16).map_err(|e| {
        invalid_input(format!(
            "config.apis.j2534-0404....vendor_ioctls key {key:?} is not a valid hex cmd_id: {e}"
        ))
    })?;
    // rpc_misc.rs's rpc_io_ctl only ever consults this table for
    // cmd_id >= 0x0001_0000 (the J2534-1 tool-manufacturer-specific
    // range, ADR-219) -- anything below that always takes the closed
    // per-command match arms or the legacy path instead, regardless of
    // what's configured here. Accepting a lower key would let an operator
    // configure a contract that is silently never consulted, appearing
    // active while doing nothing -- reject it at startup instead.
    if cmd_id < 0x0001_0000 {
        return Err(invalid_input(format!(
            "config.apis.j2534-0404....vendor_ioctls key {key:?} (cmd_id {cmd_id:#010x}) is \
             below the vendor range (0x00010000 and above, ADR-219) that rpc_io_ctl actually \
             consults this table for; a lower cmd_id's configured contract would never be used"
        )));
    }
    // A cmd_id in the vendor range can still collide with one of this
    // service's own ~28 PDU_IOCTL_BASE-derived private ids -- each already
    // matched by its own dedicated arm in rpc_io_ctl, ahead of the vendor
    // dispatch fallback (ADR-219's Accepted residual on this exact
    // collision only covered a future maintainer relocating one of those
    // constants into the vendor range; an operator's vendor_ioctls config
    // independently landing on one of the already-reserved values needs no
    // relocation at all to happen). Such a contract would "load"
    // successfully yet never actually be consulted for a request to this
    // cmd_id -- it would always be intercepted by the earlier, more
    // specific match arm instead (Codex review, PR #133 tenth round).
    if crate::service::is_reserved_service_ioctl_id(cmd_id) {
        return Err(invalid_input(format!(
            "config.apis.j2534-0404....vendor_ioctls key {key:?} (cmd_id {cmd_id:#010x}) \
             collides with one of this service's own reserved PDU_IOCTL_BASE ids; rpc_io_ctl \
             matches that value against its own dedicated handler before ever reaching the \
             vendor dispatch path (ADR-219), so a configured contract here would never actually \
             be used for a client request to this cmd_id"
        )));
    }
    let contract = match entry.shape.as_str() {
        "raw" => {
            // `input_required`/`output_required` are `sbyte_array`-only
            // fields -- `raw` already expresses the identical requirement
            // via a nonzero `input_bytes`/`output_bytes` instead, so a
            // `true` flag here alongside `raw` is a config author's typo
            // (they almost certainly meant to set the corresponding byte
            // count instead), not a request for a third representation of
            // the same thing. Reject at startup rather than silently
            // ignoring it.
            if entry.input_required || entry.output_required {
                return Err(invalid_input(format!(
                    "config.apis.j2534-0404....vendor_ioctls.{key:?} sets input_required/\
                     output_required, but shape is \"raw\" -- raw mode already expresses a \
                     required direction via a nonzero input_bytes/output_bytes instead \
                     (ADR-219, as amended); did you mean to set that instead?"
                )));
            }
            // Reject an infeasibly large configured size before it can ever
            // reach AlignedByteBuf::zeroed's unbounded allocation at
            // dispatch time -- see VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES's
            // own doc comment for why this is a rejection ceiling, not a
            // reintroduction of the fixed allocation cap this same PR
            // removed for raw mode.
            if entry.input_bytes > VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES {
                return Err(invalid_input(format!(
                    "config.apis.j2534-0404....vendor_ioctls.{key:?} input_bytes \
                     ({}) exceeds the {VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES}-byte sanity ceiling \
                     (ADR-219, as amended); raise VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES if a real \
                     vendor contract genuinely needs more",
                    entry.input_bytes
                )));
            }
            if entry.output_bytes > VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES {
                return Err(invalid_input(format!(
                    "config.apis.j2534-0404....vendor_ioctls.{key:?} output_bytes \
                     ({}) exceeds the {VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES}-byte sanity ceiling \
                     (ADR-219, as amended); raise VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES if a real \
                     vendor contract genuinely needs more",
                    entry.output_bytes
                )));
            }
            VendorIoctlContract::Raw {
                input_bytes: entry.input_bytes,
                output_bytes: entry.output_bytes,
            }
        }
        "sbyte_array" => {
            // `input_bytes`/`output_bytes` are `raw`-only fields -- setting
            // either alongside `sbyte_array` is silently ignored today
            // (wrapped mode has no use for them), which is exactly the kind
            // of silent-no-op a config author is unlikely to notice; reject
            // it at startup instead, the same "typo surfaces now, not
            // later" convention as the `raw`-with-required-flags check
            // above.
            if entry.input_bytes != 0 || entry.output_bytes != 0 {
                return Err(invalid_input(format!(
                    "config.apis.j2534-0404....vendor_ioctls.{key:?} sets input_bytes/\
                     output_bytes, but shape is \"sbyte_array\" -- wrapped mode's own \
                     SBYTE_ARRAY.NumOfBytes self-describes length, so these fields are never \
                     consulted for it (ADR-219, as amended); did you mean shape = \"raw\", or \
                     input_required/output_required instead?"
                )));
            }
            VendorIoctlContract::SbyteArray {
                input_required: entry.input_required,
                output_required: entry.output_required,
            }
        }
        other => {
            return Err(invalid_input(format!(
                "config.apis.j2534-0404....vendor_ioctls.{key:?}.shape {other:?} is invalid; \
                 expected \"raw\" or \"sbyte_array\" (ADR-219)"
            )));
        }
    };
    Ok((cmd_id, contract))
}

/// Resolves the `vendor_ioctls` table configured for `library_name`
/// (`config.apis.j2534-0404.libs."<lib>".vendor_ioctls`, see
/// `vci_service_config::find_vendor_ioctls`, ADR-219 as amended) into a
/// `cmd_id -> VendorIoctlContract` map, loaded and fail-fast validated once
/// at startup -- mirroring `resolve_modules`/`CanChannelMode::from_config`'s
/// same "typo surfaces at startup" convention -- instead of re-reading and
/// re-parsing `config.toml` on every vendor IOCTL dispatch.
///
/// Absent entirely: returns an empty map (every `cmd_id` is unconfigured).
/// A malformed key, an invalid `shape` value, or a config file that cannot
/// be read/parsed at all (e.g. a misspelled `input_required`-class field
/// tripping `VendorIoctlConfigEntry`'s `#[serde(deny_unknown_fields)]`):
/// a startup error.
pub(crate) fn resolve_vendor_ioctls(
    arch: Option<&str>,
    library_name: &str,
) -> Result<HashMap<u32, VendorIoctlContract>, io::Error> {
    let configured =
        vci_service_launcher::config::find_vendor_ioctls("j2534-0404", arch, library_name)
            .map_err(|e| invalid_input(format!("cannot load the vendor_ioctls config: {e}")))?;
    let Some(configured) = configured else {
        return Ok(HashMap::new());
    };
    configured
        .into_iter()
        .map(|(key, entry)| parse_vendor_ioctl_entry(&key, entry))
        .collect()
}

/// Parse the second command-line argument as `j2534-0404:<library>?port=<u16>`.
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
    let parsed = Url::parse(startup_arg)
        .map_err(|e| invalid_input(format!("invalid startup argument: {e}")))?;

    if parsed.scheme() != "j2534-0404" {
        return Err(invalid_input(
            "startup argument must use format 'j2534-0404:<library name>?port=<grpc port number>&...'",
        ));
    }

    let library_name = parsed.path().trim().to_string();
    if library_name.is_empty() {
        return Err(invalid_input(
            "startup argument must include a non-empty library name",
        ));
    }

    let mut requested_port = None;
    for (key, value) in parsed.query_pairs() {
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
        library_name,
        requested_port,
    })
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::{
        VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES, VendorIoctlContract, build_module_entries,
        parse_startup_arg, parse_vendor_ioctl_entry,
    };
    use vci_service_launcher::config::{ModuleConfigEntry, VendorIoctlConfigEntry};

    #[test]
    fn build_module_entries_none_synthesizes_default_single_module() {
        let entries = build_module_entries(None).expect("should synthesize a default entry");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "j2534-0404");
        assert!(entries[0].pname.is_none());
    }

    #[test]
    fn build_module_entries_empty_array_is_a_startup_error() {
        let err = build_module_entries(Some(Vec::new()))
            .expect_err("an empty modules array should be rejected");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn build_module_entries_converts_pname_to_cstring() {
        let entries = build_module_entries(Some(vec![
            ModuleConfigEntry {
                label: "Bench 1".to_string(),
                pname: "USB:1".to_string(),
            },
            ModuleConfigEntry {
                label: "Bench 2".to_string(),
                pname: "USB:2".to_string(),
            },
        ]))
        .expect("valid ASCII pnames should be accepted");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "Bench 1");
        assert_eq!(entries[0].pname.as_deref(), Some(c"USB:1"));
        assert_eq!(entries[1].label, "Bench 2");
        assert_eq!(entries[1].pname.as_deref(), Some(c"USB:2"));
    }

    #[test]
    fn build_module_entries_rejects_non_ascii_pname() {
        let err = build_module_entries(Some(vec![ModuleConfigEntry {
            label: "Bench 1".to_string(),
            pname: "USB:é".to_string(),
        }]))
        .expect_err("a non-ASCII pname should be rejected");
        assert!(err.to_string().contains("ASCII"));
    }

    #[test]
    fn build_module_entries_rejects_embedded_nul_in_pname() {
        let err = build_module_entries(Some(vec![ModuleConfigEntry {
            label: "Bench 1".to_string(),
            pname: "USB:1\x001".to_string(),
        }]))
        .expect_err("an embedded NUL byte in pname should be rejected");
        assert!(err.to_string().contains("NUL"));
    }

    #[test]
    fn parse_startup_config_accepts_library_without_query() {
        let config = parse_startup_arg("j2534-0404:demo-lib").unwrap();
        assert_eq!(config.library_name, "demo-lib");
        assert_eq!(config.requested_port, None);
    }

    #[test]
    fn parse_startup_config_accepts_optional_port_query() {
        let config = parse_startup_arg("j2534-0404:demo-lib?port=60123&foo=bar").unwrap();
        assert_eq!(config.library_name, "demo-lib");
        assert_eq!(config.requested_port, Some(60123));
    }

    #[test]
    fn parse_startup_config_rejects_empty_library_name() {
        let err = parse_startup_arg("j2534-0404:").expect_err("missing library should fail");
        assert!(err.to_string().contains("non-empty library name"));
    }

    #[test]
    fn parse_startup_config_rejects_non_j2534_scheme() {
        let err = parse_startup_arg("one-liner://demo-lib").expect_err("wrong scheme should fail");
        assert!(err.to_string().contains("j2534-0404"));
    }

    #[test]
    fn parse_startup_config_rejects_duplicate_port() {
        let err = parse_startup_arg("j2534-0404:demo-lib?port=1234&port=5678")
            .expect_err("duplicate port should fail");
        assert!(err.to_string().contains("must not repeat"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_carries_input_output_bytes() {
        let (cmd_id, contract) = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 8,
                input_required: false,
                output_required: false,
            },
        )
        .expect("a well-formed raw entry should parse");
        assert_eq!(cmd_id, 0x0001_0001);
        assert_eq!(
            contract,
            VendorIoctlContract::Raw {
                input_bytes: 4,
                output_bytes: 8,
            }
        );
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_accepts_output_bytes_at_the_sanity_ceiling() {
        // Locks in the third-round decision this ceiling must not
        // reintroduce: a real large-but-reasonable buffer (well above the
        // old, removed 64 KiB cap) is still honored exactly as configured.
        let (_, contract) = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES,
                input_required: false,
                output_required: false,
            },
        )
        .expect("output_bytes exactly at the ceiling must still be accepted");
        assert_eq!(
            contract,
            VendorIoctlContract::Raw {
                input_bytes: 0,
                output_bytes: VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES,
            }
        );
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_accepts_input_bytes_at_the_sanity_ceiling() {
        // Mirrors the output_bytes case above: the ceiling applies
        // symmetrically to both directions, not just output_bytes.
        let (_, contract) = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            },
        )
        .expect("input_bytes exactly at the ceiling must still be accepted");
        assert_eq!(
            contract,
            VendorIoctlContract::Raw {
                input_bytes: VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES,
                output_bytes: 0,
            }
        );
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_still_accepts_a_realistic_large_buffer() {
        let (_, contract) = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: 100_000,
                input_required: false,
                output_required: false,
            },
        )
        .expect("100_000 bytes -- well above the old, removed 64 KiB cap -- must still parse");
        assert_eq!(
            contract,
            VendorIoctlContract::Raw {
                input_bytes: 0,
                output_bytes: 100_000,
            }
        );
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_rejects_output_bytes_above_the_sanity_ceiling() {
        let err = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES + 1,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("output_bytes one above the ceiling should be rejected at startup");
        assert!(err.to_string().contains("sanity ceiling"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_rejects_input_bytes_above_the_sanity_ceiling() {
        let err = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES + 1,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("input_bytes one above the ceiling should be rejected at startup");
        assert!(err.to_string().contains("sanity ceiling"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_rejects_output_bytes_at_u32_max() {
        // The exact scenario Codex's finding described: an absurd
        // syntactically-valid value that would otherwise reach an
        // unbounded allocation at dispatch time.
        let err = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: u32::MAX,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("output_bytes = u32::MAX should be rejected at startup");
        assert!(err.to_string().contains("sanity ceiling"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_sbyte_array_shape_parses_required_flags() {
        let (cmd_id, contract) = parse_vendor_ioctl_entry(
            "0x00010002",
            VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: true,
                output_required: false,
            },
        )
        .expect("a well-formed sbyte_array entry should parse");
        assert_eq!(cmd_id, 0x0001_0002);
        assert_eq!(
            contract,
            VendorIoctlContract::SbyteArray {
                input_required: true,
                output_required: false,
            }
        );
    }

    #[test]
    fn parse_vendor_ioctl_entry_sbyte_array_shape_rejects_nonzero_input_bytes() {
        let err = parse_vendor_ioctl_entry(
            "0x00010002",
            VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 4,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("sbyte_array with a nonzero input_bytes should be rejected at startup");
        assert!(err.to_string().contains("input_bytes"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_sbyte_array_shape_rejects_nonzero_output_bytes() {
        let err = parse_vendor_ioctl_entry(
            "0x00010002",
            VendorIoctlConfigEntry {
                shape: "sbyte_array".to_string(),
                input_bytes: 0,
                output_bytes: 8,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("sbyte_array with a nonzero output_bytes should be rejected at startup");
        assert!(err.to_string().contains("output_bytes"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_rejects_input_required() {
        let err = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: true,
                output_required: false,
            },
        )
        .expect_err("raw with input_required set should be rejected at startup");
        assert!(err.to_string().contains("input_required"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_raw_shape_rejects_output_required() {
        let err = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: true,
            },
        )
        .expect_err("raw with output_required set should be rejected at startup");
        assert!(err.to_string().contains("output_required"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_rejects_invalid_shape() {
        let err = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "weird".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("an invalid shape string should be rejected at startup");
        assert!(err.to_string().contains("weird"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_rejects_key_without_0x_prefix() {
        let err = parse_vendor_ioctl_entry(
            "00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("a key without the 0x prefix should be rejected at startup");
        assert!(err.to_string().contains("8 lowercase hex digits"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_rejects_non_hex_key() {
        let err = parse_vendor_ioctl_entry(
            "0xZZZZZZZZ",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("a non-hex key should be rejected at startup");
        assert!(err.to_string().contains("8 lowercase hex digits"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_rejects_a_shorter_alias_of_the_same_cmd_id() {
        // "0x10001" and "0x00010001" would otherwise decode to the same
        // u32 cmd_id -- accepting both lets two aliases of one command
        // silently collapse into a single HashMap entry, with whichever
        // is inserted last (a source-map iteration order this service
        // does not control) winning, defeating the fail-fast guarantee
        // the whole vendor_ioctls table exists for.
        let err = parse_vendor_ioctl_entry(
            "0x10001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 4,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("a non-canonical-length key should be rejected at startup");
        assert!(err.to_string().contains("8 lowercase hex digits"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_rejects_an_uppercase_hex_key() {
        let err = parse_vendor_ioctl_entry(
            "0x000100AB",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 4,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("an uppercase-hex key should be rejected at startup");
        assert!(err.to_string().contains("8 lowercase hex digits"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_rejects_a_cmd_id_below_the_vendor_range() {
        // rpc_io_ctl only ever consults this table for cmd_id >= 0x10000;
        // a lower key would parse successfully but its contract would
        // never actually be looked up by dispatch, silently doing nothing
        // while appearing configured.
        let err = parse_vendor_ioctl_entry(
            "0x00000001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 4,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("a cmd_id below the vendor range should be rejected at startup");
        assert!(err.to_string().contains("below the vendor range"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_rejects_a_cmd_id_reserved_by_a_service_ioctl_handler() {
        // 0x29000001 is PDU_IOCTL_RESET (PDU_IOCTL_BASE + 0x01) -- inside
        // the vendor range (>= 0x10000), so the below-vendor-range check
        // alone would accept it, but rpc_io_ctl's own PDU_IOCTL_RESET arm
        // would intercept every request to this cmd_id before it ever
        // reached the vendor dispatch path (Codex review, PR #133 tenth round).
        let err = parse_vendor_ioctl_entry(
            "0x29000001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 0,
                output_bytes: 0,
                input_required: false,
                output_required: false,
            },
        )
        .expect_err("a cmd_id reserved by a service IOCTL handler should be rejected at startup");
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn parse_vendor_ioctl_entry_accepts_the_vendor_range_floor() {
        let (cmd_id, _) = parse_vendor_ioctl_entry(
            "0x00010000",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 4,
                input_required: false,
                output_required: false,
            },
        )
        .expect("the exact vendor range floor (0x10000) must still be accepted");
        assert_eq!(cmd_id, 0x0001_0000);
    }

    #[test]
    fn parse_vendor_ioctl_entry_accepts_the_canonical_eight_digit_lowercase_key() {
        let (cmd_id, _) = parse_vendor_ioctl_entry(
            "0x00010001",
            VendorIoctlConfigEntry {
                shape: "raw".to_string(),
                input_bytes: 4,
                output_bytes: 4,
                input_required: false,
                output_required: false,
            },
        )
        .expect("the canonical grammar must still be accepted");
        assert_eq!(cmd_id, 0x0001_0001);
    }
}
