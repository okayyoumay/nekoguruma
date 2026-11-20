//! The Linux user-mode key store: the agent key as an item of the freedesktop Secret Service
//! (design 6.4, ADR-459 Decision item 4, ADR-460).
//!
//! Two layers. [`Transport`] is the six D-Bus calls the store makes, each answering with a fixed
//! reason when it fails; the policy ([`get_with`], [`put_new_with`]) turns their results into
//! [`StoreErrorKind`]s and is tested with a fake transport. [`DbusTransport`] is the real one: a
//! session-bus connection opened for one `get` or `put_new` and closed after it, never cached.
//!
//! The store only reads and creates. It never unlocks a collection, creates one, sets an alias
//! or locks one, and a prompt the service offers is not run: a locked or missing collection is
//! [`StoreErrorKind::Unavailable`] and the user unlocks it outside the agent. The item is found
//! by one attribute holding the secret's name, service-wide, so a key a user moved to another
//! collection is still found; the collection a new item goes to is the `default` alias.
//!
//! The session algorithm is `plain`: the key crosses the bus unencrypted, which the bus's
//! same-account trust allows (the account's own processes can read the key from the unlocked
//! collection anyway). Error sources are fixed strings, never the daemon's text, which may echo
//! what the call carried.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use shared_crypto::Zeroizing;
use zbus::connection::Builder;
use zbus::{Connection, OwnedObjectPath, Variant};

use super::store::{SecretName, SecretStore, StoreError, StoreErrorKind};

const DEST: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// The attribute that holds the secret's name; the one attribute the store searches on.
const NAME_ATTRIBUTE: &str = "nekoguruma-secret-name";
/// The item's label, for the user's key manager; display only.
const LABEL: &str = "Nekoguruma agent key (user)";
/// The content type written with the key (the PKCS#8 DER bytes).
const CONTENT_TYPE: &str = "application/octet-stream";

/// How long one method call may take before it counts as unanswered.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a call failed, in words fixed in this file.
type Reason = &'static str;

/// What a search found: the item paths, split by their collection's lock state.
#[derive(Debug, Default)]
struct Found {
    unlocked: Vec<String>,
    locked: Vec<String>,
}

/// What `GetSecret` answered. Under the `plain` algorithm the parameters must be empty.
struct SecretReply {
    parameters: Vec<u8>,
    value: Zeroizing<Vec<u8>>,
}

/// What `CreateItem` answered.
#[derive(Debug, PartialEq, Eq)]
enum Created {
    Done,
    /// The service wants a prompt run, which the agent never does.
    Prompt,
}

/// The calls the store makes, on one connection.
trait Transport {
    /// `Service.SearchItems` on the name attribute.
    fn search(&self, name: &SecretName) -> Result<Found, Reason>;
    /// `Service.ReadAlias("default")`: the collection's path, or `None` when there is none.
    fn default_collection(&self) -> Result<Option<String>, Reason>;
    /// The collection's `Locked` property.
    fn collection_locked(&self, collection: &str) -> Result<bool, Reason>;
    /// `Service.OpenSession("plain")`: the session's path.
    fn open_session(&self) -> Result<String, Reason>;
    /// `Item.GetSecret(session)`.
    fn get_secret(&self, item: &str, session: &str) -> Result<SecretReply, Reason>;
    /// `Collection.CreateItem(.., replace = false)`.
    fn create_item(
        &self,
        collection: &str,
        session: &str,
        name: &SecretName,
        secret: &[u8],
    ) -> Result<Created, Reason>;
}

fn unavailable(reason: Reason) -> StoreError {
    StoreError::with_source(StoreErrorKind::Unavailable, reason)
}

/// The distinct item paths of both lists.
fn distinct(found: &Found) -> HashSet<&str> {
    found
        .unlocked
        .iter()
        .chain(&found.locked)
        .map(String::as_str)
        .collect()
}

fn get_with(
    transport: &dyn Transport,
    name: &SecretName,
) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let found = transport.search(name).map_err(unavailable)?;
    let items = distinct(&found);
    // Two items under one name: the agent cannot know which is its key, and never picks one.
    if items.len() > 1 {
        tracing::warn!(
            count = items.len(),
            "the Secret Service holds several items for the agent key; none is used"
        );
        return Err(StoreError::with_source(
            StoreErrorKind::Undecryptable,
            "several items for one name",
        ));
    }
    if let Some(item) = found.unlocked.first() {
        let session = transport.open_session().map_err(unavailable)?;
        // An item deleted between the search and this call is a failed read, not an absent key.
        let reply = transport.get_secret(item, &session).map_err(unavailable)?;
        if !reply.parameters.is_empty() {
            return Err(unavailable("unexpected session parameters"));
        }
        return Ok(reply.value);
    }
    if !items.is_empty() {
        return Err(unavailable("item locked"));
    }
    // Nothing matched. That is an answer of "absent" only when the collection a key would be
    // created in exists and is unlocked: a locked collection may hide its items from a search.
    let collection = transport
        .default_collection()
        .map_err(unavailable)?
        .ok_or_else(|| unavailable("no default collection"))?;
    if transport
        .collection_locked(&collection)
        .map_err(unavailable)?
    {
        return Err(unavailable("collection locked"));
    }
    Err(StoreError::new(StoreErrorKind::Absent))
}

fn put_new_with(
    transport: &dyn Transport,
    name: &SecretName,
    secret: &[u8],
) -> Result<(), StoreError> {
    let found = transport.search(name).map_err(unavailable)?;
    if !distinct(&found).is_empty() {
        return Err(StoreError::new(StoreErrorKind::AlreadyExists));
    }
    let collection = transport
        .default_collection()
        .map_err(unavailable)?
        .ok_or_else(|| unavailable("no default collection"))?;
    if transport
        .collection_locked(&collection)
        .map_err(unavailable)?
    {
        return Err(unavailable("collection locked"));
    }
    let session = transport.open_session().map_err(unavailable)?;
    // A create that fails on the wire may still have happened; the caller reads back.
    match transport
        .create_item(&collection, &session, name, secret)
        .map_err(unavailable)?
    {
        Created::Done => Ok(()),
        Created::Prompt => Err(unavailable("the service asked for a prompt")),
    }
}

/// The user-mode key store on the freedesktop Secret Service. It holds no connection: each call
/// opens one and closes it.
#[derive(Debug, Default)]
pub struct SecretService;

impl SecretService {
    /// A store; it does no I/O until the first call.
    pub fn new() -> Self {
        Self
    }
}

impl SecretStore for SecretService {
    fn get(&self, name: &SecretName) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        get_with(&DbusTransport::connect().map_err(unavailable)?, name)
    }

    fn put_new(&self, name: &SecretName, secret: &[u8]) -> Result<(), StoreError> {
        put_new_with(
            &DbusTransport::connect().map_err(unavailable)?,
            name,
            secret,
        )
    }
}

/// One session-bus connection. zbus 6 has no blocking API, so every call runs `block_on` over
/// the async one; the callers are already off the async tasks.
struct DbusTransport {
    connection: Option<Connection>,
}

impl DbusTransport {
    fn connect() -> Result<Self, Reason> {
        let connection = zbus::block_on(async {
            Builder::session()
                .method_timeout(CALL_TIMEOUT)
                .build()
                .await
        })
        .map_err(|_| "no session bus")?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    fn connection(&self) -> &Connection {
        self.connection.as_ref().expect("open until dropped")
    }

    /// One method call, answered as `R`; every failure becomes a fixed reason.
    fn call<B, R>(
        &self,
        path: &str,
        interface: &'static str,
        method: &'static str,
        body: &B,
    ) -> Result<R, Reason>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        zbus::block_on(async {
            let reply = self
                .connection()
                .call_method(Some(DEST), path, Some(interface), method, body)
                .await?;
            reply.body().deserialize::<R>()
        })
        .map_err(|error| reason(&error))
    }
}

impl Drop for DbusTransport {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            // Best effort: the session dies with the connection either way.
            let _ = zbus::block_on(connection.close());
        }
    }
}

/// The fixed reason for a failed call. The daemon's own text is dropped.
fn reason(error: &zbus::Error) -> Reason {
    use zbus::fdo::Error as Fdo;
    match error {
        zbus::Error::FDO(fdo) => match &**fdo {
            Fdo::ServiceUnknown(_) | Fdo::NameHasNoOwner(_) => "no Secret Service",
            Fdo::NoReply(_) | Fdo::Timeout(_) | Fdo::TimedOut(_) => "no answer in time",
            Fdo::UnknownObject(_) => "no such object",
            Fdo::Disconnected(_) | Fdo::IOError(_) => "connection lost",
            _ => "Secret Service call failed",
        },
        zbus::Error::MethodError(name, _, _) => match name.rsplit('.').next() {
            Some("IsLocked") => "item locked",
            Some("NoSession") => "no session",
            Some("NoSuchObject") => "no such object",
            _ => "Secret Service call failed",
        },
        zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut => {
            "no answer in time"
        }
        zbus::Error::InputOutput(_) => "connection lost",
        _ => "Secret Service call failed",
    }
}

fn attributes(name: &SecretName) -> HashMap<&'static str, &str> {
    HashMap::from([(NAME_ATTRIBUTE, name.as_str())])
}

fn paths(list: Vec<OwnedObjectPath>) -> Vec<String> {
    list.iter().map(|path| path.as_str().to_owned()).collect()
}

impl Transport for DbusTransport {
    fn search(&self, name: &SecretName) -> Result<Found, Reason> {
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = self.call(
            SERVICE_PATH,
            SERVICE_IFACE,
            "SearchItems",
            &(attributes(name),),
        )?;
        Ok(Found {
            unlocked: paths(unlocked),
            locked: paths(locked),
        })
    }

    fn default_collection(&self) -> Result<Option<String>, Reason> {
        let (path,): (OwnedObjectPath,) =
            self.call(SERVICE_PATH, SERVICE_IFACE, "ReadAlias", &("default",))?;
        // `/` is the service's answer for "no such alias".
        Ok((path.as_str() != "/").then(|| path.as_str().to_owned()))
    }

    fn collection_locked(&self, collection: &str) -> Result<bool, Reason> {
        let (value,): (zbus::OwnedVariant,) = self.call(
            collection,
            PROPERTIES_IFACE,
            "Get",
            &(COLLECTION_IFACE, "Locked"),
        )?;
        bool::try_from(value).map_err(|_| "Secret Service call failed")
    }

    fn open_session(&self) -> Result<String, Reason> {
        let (_output, session): (zbus::OwnedVariant, OwnedObjectPath) = self.call(
            SERVICE_PATH,
            SERVICE_IFACE,
            "OpenSession",
            &("plain", Variant::from("")),
        )?;
        Ok(session.as_str().to_owned())
    }

    fn get_secret(&self, item: &str, session: &str) -> Result<SecretReply, Reason> {
        let session = zbus::ObjectPath::try_from(session).map_err(|_| "bad session")?;
        let ((_session, parameters, value, _content_type),): ((
            OwnedObjectPath,
            Vec<u8>,
            Vec<u8>,
            String,
        ),) = self.call(item, ITEM_IFACE, "GetSecret", &(session,))?;
        Ok(SecretReply {
            parameters,
            value: Zeroizing::new(value),
        })
    }

    fn create_item(
        &self,
        collection: &str,
        session: &str,
        name: &SecretName,
        secret: &[u8],
    ) -> Result<Created, Reason> {
        let session = zbus::ObjectPath::try_from(session).map_err(|_| "bad session")?;
        let properties: HashMap<&str, Variant> = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Variant::from(LABEL)),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Variant::from(attributes(name)),
            ),
        ]);
        let value = (session, Vec::<u8>::new(), secret, CONTENT_TYPE);
        let (_item, prompt): (OwnedObjectPath, OwnedObjectPath) = self.call(
            collection,
            COLLECTION_IFACE,
            "CreateItem",
            &(properties, value, false),
        )?;
        Ok(if prompt.as_str() == "/" {
            Created::Done
        } else {
            Created::Prompt
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A scripted transport: each field is the answer of its call, and `calls` records the
    /// calls made.
    struct Fake {
        search: Result<(Vec<&'static str>, Vec<&'static str>), Reason>,
        default: Result<Option<&'static str>, Reason>,
        locked: Result<bool, Reason>,
        session: Result<&'static str, Reason>,
        secret: Result<(Vec<u8>, Vec<u8>), Reason>,
        create: Result<Created, Reason>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        /// An unlocked default collection holding nothing, everything answering.
        fn empty() -> Self {
            Self {
                search: Ok((vec![], vec![])),
                default: Ok(Some("/c/default")),
                locked: Ok(false),
                session: Ok("/s/1"),
                secret: Ok((vec![], b"key".to_vec())),
                create: Ok(Created::Done),
                calls: RefCell::new(vec![]),
            }
        }

        fn with_items(unlocked: &[&'static str], locked: &[&'static str]) -> Self {
            Self {
                search: Ok((unlocked.to_vec(), locked.to_vec())),
                ..Self::empty()
            }
        }

        fn called(&self, call: &'static str) -> bool {
            self.calls.borrow().contains(&call)
        }
    }

    impl Transport for Fake {
        fn search(&self, _: &SecretName) -> Result<Found, Reason> {
            self.calls.borrow_mut().push("search");
            self.search.clone().map(|(unlocked, locked)| Found {
                unlocked: unlocked.iter().map(|p| p.to_string()).collect(),
                locked: locked.iter().map(|p| p.to_string()).collect(),
            })
        }

        fn default_collection(&self) -> Result<Option<String>, Reason> {
            self.calls.borrow_mut().push("default");
            self.default.map(|path| path.map(str::to_owned))
        }

        fn collection_locked(&self, _: &str) -> Result<bool, Reason> {
            self.calls.borrow_mut().push("locked");
            self.locked
        }

        fn open_session(&self) -> Result<String, Reason> {
            self.calls.borrow_mut().push("session");
            self.session.map(str::to_owned)
        }

        fn get_secret(&self, _: &str, _: &str) -> Result<SecretReply, Reason> {
            self.calls.borrow_mut().push("get_secret");
            self.secret.clone().map(|(parameters, value)| SecretReply {
                parameters,
                value: Zeroizing::new(value),
            })
        }

        fn create_item(
            &self,
            _: &str,
            _: &str,
            _: &SecretName,
            _: &[u8],
        ) -> Result<Created, Reason> {
            self.calls.borrow_mut().push("create");
            match &self.create {
                Ok(Created::Done) => Ok(Created::Done),
                Ok(Created::Prompt) => Ok(Created::Prompt),
                Err(reason) => Err(reason),
            }
        }
    }

    fn name() -> SecretName {
        SecretName::new("nekoguruma-agent-key.user.v1").unwrap()
    }

    fn get_kind(fake: &Fake) -> Result<Vec<u8>, StoreErrorKind> {
        get_with(fake, &name())
            .map(|bytes| bytes.to_vec())
            .map_err(|error| error.kind())
    }

    fn put_kind(fake: &Fake) -> Result<(), StoreErrorKind> {
        put_new_with(fake, &name(), b"new").map_err(|error| error.kind())
    }

    use StoreErrorKind::{Absent, AlreadyExists, Unavailable, Undecryptable};

    /// Row 4: a session the service refuses.
    #[test]
    fn a_refused_session_is_unavailable() {
        let fake = Fake {
            session: Err("Secret Service call failed"),
            ..Fake::with_items(&["/i/1"], &[])
        };
        assert_eq!(get_kind(&fake), Err(Unavailable));
        let fake = Fake {
            session: Err("Secret Service call failed"),
            ..Fake::empty()
        };
        assert_eq!(put_kind(&fake), Err(Unavailable));
        assert!(!fake.called("create"));
    }

    /// Row 5 with a failing search (rows 1 to 3 reach the policy as a failed first call).
    #[test]
    fn a_failed_search_is_unavailable() {
        for reason in ["no Secret Service", "no answer in time", "connection lost"] {
            let fake = Fake {
                search: Err(reason),
                ..Fake::empty()
            };
            assert_eq!(get_kind(&fake), Err(Unavailable));
            assert_eq!(put_kind(&fake), Err(Unavailable));
            let error = get_with(&fake, &name()).unwrap_err();
            assert!(error.to_string().ends_with(&format!("({reason})")));
        }
    }

    /// Row 5: no default collection and nothing found is never "absent", and nothing is created.
    #[test]
    fn no_default_collection_is_unavailable_and_creates_nothing() {
        let fake = Fake {
            default: Ok(None),
            ..Fake::empty()
        };
        assert_eq!(get_kind(&fake), Err(Unavailable));
        assert_eq!(put_kind(&fake), Err(Unavailable));
        assert!(!fake.called("create"));
    }

    /// Row 6: an item in some other unlocked collection is found with no default collection.
    #[test]
    fn an_item_is_found_without_a_default_collection() {
        let fake = Fake {
            default: Ok(None),
            ..Fake::with_items(&["/c/other/1"], &[])
        };
        assert_eq!(get_kind(&fake), Ok(b"key".to_vec()));
        assert_eq!(put_kind(&fake), Err(AlreadyExists));
    }

    /// Row 7: a locked default collection that hides its items.
    #[test]
    fn a_locked_default_collection_that_hides_its_items_is_unavailable() {
        let fake = Fake {
            locked: Ok(true),
            ..Fake::empty()
        };
        assert_eq!(get_kind(&fake), Err(Unavailable));
        assert_eq!(put_kind(&fake), Err(Unavailable));
        assert!(!fake.called("create"));
    }

    /// Rows 8 and 11: an item in a locked list is unavailable to read and counts as present.
    #[test]
    fn a_locked_item_is_unavailable_to_read_and_blocks_a_create() {
        let fake = Fake::with_items(&[], &["/c/x/1"]);
        assert_eq!(get_kind(&fake), Err(Unavailable));
        assert_eq!(put_kind(&fake), Err(AlreadyExists));
        assert!(!fake.called("get_secret") && !fake.called("create"));
    }

    /// Row 9: nothing found in an unlocked default collection is absent, and a create is made.
    #[test]
    fn nothing_in_an_unlocked_default_collection_is_absent_and_creatable() {
        let fake = Fake::empty();
        assert_eq!(get_kind(&fake), Err(Absent));
        assert_eq!(put_kind(&fake), Ok(()));
        assert!(fake.called("create"));
    }

    /// Row 10.
    #[test]
    fn one_unlocked_item_is_read_and_blocks_a_create() {
        let fake = Fake::with_items(&["/i/1"], &[]);
        assert_eq!(get_kind(&fake), Ok(b"key".to_vec()));
        assert_eq!(put_kind(&fake), Err(AlreadyExists));
    }

    /// Row 12: two distinct items, wherever they are listed, are undecryptable; the same path
    /// listed twice is one item.
    #[test]
    fn several_items_are_undecryptable_and_block_a_create() {
        let cases: [(&[&'static str], &[&'static str]); 3] = [
            (&["/i/1", "/i/2"], &[]),
            (&[], &["/i/1", "/i/2"]),
            (&["/i/1"], &["/i/2"]),
        ];
        for (unlocked, locked) in cases {
            let fake = Fake::with_items(unlocked, locked);
            assert_eq!(get_kind(&fake), Err(Undecryptable));
            assert_eq!(put_kind(&fake), Err(AlreadyExists));
            assert!(!fake.called("get_secret"));
        }
        let fake = Fake::with_items(&["/i/1"], &["/i/1"]);
        assert_eq!(get_kind(&fake), Ok(b"key".to_vec()));
    }

    /// Rows 13 to 15: a secret that cannot be fetched, whatever the reason, is unavailable and
    /// never absent.
    #[test]
    fn a_failed_get_secret_is_unavailable_never_absent() {
        for reason in [
            "item locked",
            "no such object",
            "no session",
            "no answer in time",
        ] {
            let fake = Fake {
                secret: Err(reason),
                ..Fake::with_items(&["/i/1"], &[])
            };
            assert_eq!(get_kind(&fake), Err(Unavailable), "{reason}");
        }
    }

    /// Row 16: parameters under the plain algorithm.
    #[test]
    fn parameters_under_plain_are_unavailable() {
        let fake = Fake {
            secret: Ok((vec![1], b"key".to_vec())),
            ..Fake::with_items(&["/i/1"], &[])
        };
        assert_eq!(get_kind(&fake), Err(Unavailable));
    }

    /// Row 17: a create that times out or loses the connection.
    #[test]
    fn a_create_that_fails_on_the_wire_is_unavailable() {
        for reason in ["no answer in time", "connection lost"] {
            let fake = Fake {
                create: Err(reason),
                ..Fake::empty()
            };
            assert_eq!(put_kind(&fake), Err(Unavailable));
        }
    }

    /// Row 18: a prompt path in the answer is not run.
    #[test]
    fn a_prompt_is_unavailable() {
        let fake = Fake {
            create: Ok(Created::Prompt),
            ..Fake::empty()
        };
        assert_eq!(put_kind(&fake), Err(Unavailable));
    }

    /// Row 20: whatever bytes the item holds come back as stored; the identity code judges them.
    #[test]
    fn the_bytes_come_back_as_stored() {
        for stored in [vec![], vec![0u8; 3], b"not pkcs8".to_vec()] {
            let fake = Fake {
                secret: Ok((vec![], stored.clone())),
                ..Fake::with_items(&["/i/1"], &[])
            };
            assert_eq!(get_kind(&fake), Ok(stored));
        }
    }

    /// Rows 1 and 2 through the real code: a failed call maps to a fixed reason, not the text.
    #[test]
    fn a_failed_call_gives_a_fixed_reason() {
        let error = zbus::Error::Failure("secret text".into());
        assert_eq!(reason(&error), "Secret Service call failed");
        let error = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown("x".into())));
        assert_eq!(reason(&error), "no Secret Service");
        let error = zbus::Error::FDO(Box::new(zbus::fdo::Error::NoReply("x".into())));
        assert_eq!(reason(&error), "no answer in time");
    }
}
