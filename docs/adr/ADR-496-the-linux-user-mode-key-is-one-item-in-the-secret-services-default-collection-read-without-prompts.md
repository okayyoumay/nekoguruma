# ADR-496: The Linux User-Mode Key Is One Item in the Secret Service's Default Collection, Read Without Prompts

**Date:** 2026-11-20
**Status:** Accepted
**Affects:** design 6.4; `crates/agent` (`identity::secret_service`, the store selection in `identity`), `crates/agent/docs/ngr-agent.md`, `.github/workflows/ci.yml` (`core-linux`), `.config/nextest.toml`, `.github/dependabot.yml`, `CLAUDE.md` ("Building and testing"); details ADR-459 item 4 and ADR-460

## Context

ADR-459 item 4 puts the agent key of Linux user mode in the Secret Service, through `zbus`, and
says that a locked collection reads as unavailable, never as absent. ADR-460 gives the store's
interface: a read answers absent only when the store positively holds nothing, and a create never
replaces. Neither fixes which collection holds the key, how the secret crosses the bus, what
identifies the item, what the agent does when the service asks for a prompt, or how the builds
select the store. The Secret Service cannot create an item only if none exists, so two processes
of one account that resolve different state directories (two lock files, ADR-460) can create two
items. And `zbus` 6, the maintained line, has no blocking API, while the store's interface is
blocking. The maintainer decided these on 2026-11-20, from a case table checked against
gnome-keyring 46.1.

## Decision

1. **`zbus` 6, a Linux-only dependency, with its default runtime only.** Each read or create
   runs one `zbus::block_on` over the asynchronous calls, on a new session-bus connection, with
   a 5-second bound on setting up the connection and on each call; nothing is cached, so a daemon restart or a lost bus needs no
   recovery code. Its `tokio` and `tracing` features stay off, so it cannot log a message that
   carries the key. `zbus` updates are left out of Dependabot's automatically merged group and
   wait for the maintainer.
2. **The `plain` session.** The session bus is a per-user socket authenticated by the peer's
   credentials, so the only observer of the secret in transit is a process of the same account,
   which could ask the service for the secret itself (ADR-459 Consequences). The Diffie-Hellman
   session does not authenticate the service and so stops no such process either, and it would
   add a 1024-bit group and unauthenticated AES-CBC beside `shared-crypto`.
3. **One item, one attribute.** The item is identified by `nekoguruma-secret-name` holding the
   secret's name (`nekoguruma-agent-key.user.v1`, ADR-460); the label is for display only. The
   value is the key's PKCS#8 DER, and the content type the service returns is ignored. Nothing
   ties the item to a state directory: one account is one agent identity (design 6.2), and a tie
   would give it two, or orphan the key when the state directory moves.
4. **Searched everywhere, created only in the default collection.** A read searches the whole
   service on the attribute. A create goes to the collection the `default` alias names, with
   `replace` false, after a search finds nothing. The store never creates a collection or moves
   the alias.
5. **Three answers that are not "unavailable".**
   - Absent: the search finds nothing, the `default` alias names a collection, and that
     collection is unlocked.
   - Undecryptable: the search finds two or more items, counted over the locked and the unlocked
     ones alike, before their lock state is looked at. Unlocking would not repair it, so the
     agent reports the key unusable and never overwrites (ADR-460); one warning gives the count.
   - Already exists: a create whose search finds any item.
   Everything else is unavailable, which the agent retries: no session bus, no Secret Service, a
   call that does not answer in time, no default collection, a locked default collection, an item
   that is locked or disappears between the search and the read, a session other than `plain`,
   and a create that fails or answers with a prompt.
6. **No prompts.** The store never unlocks, never creates a collection and never runs a prompt
   the service returns: a prompt is unbounded human time inside the agent's 10-second creation
   lock (ADR-460), and the connect loop's retries would raise it again and again. A keyring that
   is locked keeps the agent unavailable until something outside it unlocks the keyring.
7. **Selection.** A release build on Linux selects the store in user mode, with no variable read;
   device mode and the other platforms are unchanged. A debug build selects it only when
   `NGR_AGENT_KEY_STORE` is `secret-service` and the mode is user, so a test never writes the
   production item into a developer's keyring by accident; the debug plain-file store keeps its
   own variables.
8. **Tested against gnome-keyring.** The tests that talk to a real Secret Service run only when
   `NGR_TEST_SECRET_SERVICE` is set, and then fail, not skip, when the bus or the service is
   missing. `core-linux` runs them under `dbus-run-session` with gnome-keyring unlocked, one at a
   time; the rules that map each answer to an outcome are tested against a scripted transport
   everywhere.

## Consequences

- A PC with automatic login whose keyring is not unlocked at login keeps the agent unavailable
  in user mode until the keyring is unlocked; an interactive command that may run the service's
  own prompt is later work.
- Two state directories of one account can still create two items; every later start then
  reports the key unusable (exit 4). Registering again does not repair it, since a create finds
  the items already there: one of them has to be deleted by hand (for example with Seahorse)
  first.
- Only gnome-keyring is tested, and it lists locked items in a search, which the rule for absent
  rests on. A provider that leaves locked items out of a search (KeePassXC, KWallet and oo7 are
  untested) would answer absent while the key sits in a locked collection other than the
  default one, and the agent would create a second key, which every later start then reports
  as unusable. Elsewhere the mapping fails closed to unavailable.
- The key crosses the session bus unencrypted, inside the account's own trust boundary.
- `zbus` 6 is young and releases often; each update is a pull request the maintainer merges.
