# ADR-498: The Linux Device-Mode Key Is a systemd Credential Only an Administrator Provisions

**Date:** 2026-11-21
**Status:** Accepted
**Affects:** design 6.4, 6.7; `crates/agent` (`identity::systemd_credential`, `identity::provision`, `SecretStore::can_create`, the store selection in `identity`, `ngr-agent provision-key`), `crates/agent/docs/ngr-agent.md`, `packaging/systemd/ngr-agent.service`; details ADR-459 item 4 and refines ADR-460 items 4, 5 and 7

## Context

ADR-459 item 4 names a systemd encrypted credential, provisioned by the administrator, as the
Linux device-mode key store, and leaves its form open. ADR-460 gives the store interface and the
create-once routine, which takes a lock and a writable state directory before it generates a key
and calls `put_new`. ADR-496 item 7 sets how a build selects the user-mode store.

A credential is the wrong shape for that routine. The unit's `LoadCredentialEncrypted=` hands the
service a decrypted file that the service cannot write, and the file that `systemd-creds encrypt`
makes is sealed to the host (and optionally its TPM), so only an administrator, running as root,
can make it. An agent that generated a key at its first start would have nowhere to put it. The
agent in device mode must therefore read a key that already exists, report a missing one at once
(without taking a lock or creating a state directory it has no use for), and the administrator
needs a command that makes the key without the plaintext ever reaching an argument, the
environment or a file. Measured with systemd-creds 255.4: `encrypt` replaces an existing output
file silently, writes it with mode 0644, accepts empty input, and embeds the output file's name in
the credential unless `--name` is given.

The maintainer confirmed the fifteen choices below on 2026-11-21, from a case table of 16 agent,
5 lock and 12 provisioning cases.

## Decision

1. **A read-only store.** `identity::systemd_credential` reads
   `$CREDENTIALS_DIRECTORY/nekoguruma-agent-key.device.v1` and nothing else. It opens the file with
   `O_NOFOLLOW | O_NONBLOCK`, requires a regular file, and reads at most 16 KiB. A missing file in
   an existing directory is absent. A missing or non-directory credential directory, a refused
   open, a symlink, a directory, a FIFO or a device, and any other I/O error are unavailable (exit
   3). A longer file is undecryptable (exit 4), and an empty or non-key file fails the key decoder
   the same way (exit 4). `put_new` answers `NotCreatable` and touches nothing. Error text is fixed
   words and an `io::ErrorKind`: no path, no bytes.
2. **`SecretStore::can_create()`**, true by default and false for this store. When a store that
   cannot create reads absent, `load_or_create` answers `NotProvisioned` (exit 1) at once: no lock
   file, no state directory, no key generation. A present key is unchanged, and a store that claims
   `can_create` but answers `NotCreatable` still maps to `NotProvisioned`. This refines ADR-460
   items 4 and 5.
3. **`ngr-agent provision-key --mode device [--with-key auto|host|tpm2|host+tpm2]`**, Linux only,
   `auto` by default and a closed list (anything else is a usage error, exit 2). In order:
   - a release build refuses a caller who is not root (`geteuid`); `--mode user`, an installation
     file that is missing or not for the device mode, and a host that is not Linux are exit 1 (an
     installation file that cannot be read for a reason other than a refusal of access is exit 3,
     as for every command);
   - the target is `/etc/credstore.encrypted` (a debug build alone moves it with
     `NGR_AGENT_CREDSTORE_DIR`, which also lifts the root checks, so tests run as a user). A
     missing directory is created 0700, its last component only. A directory that is not owned by
     root (release), that its group or others may write, or that is a symlink is refused (exit 1);
   - anything at the final name, a dangling symlink and an empty file included (found with
     `lstat`), is exit 5, before a key is generated and before the tool is started;
   - `systemd-creds` is looked up on the absolute entries of `PATH`; without it the command exits 1
     naming systemd 250 or later, before a key is generated. A release build accepts only a file
     that, with every directory above it (links resolved), is owned by root and not writable by
     its group or others; a debug build with the moved target keeps the plain lookup. The tool runs
     with a cleared environment: `PATH`, `LANG` and `LC_*` only;
   - the key is generated as `load_or_create` does (P-256, PKCS#8) and held in `Zeroizing` memory;
   - `systemd-creds encrypt --name=nekoguruma-agent-key.device.v1 --with-key=<k> --tpm2-pcrs= - -`
     gets the plaintext on standard input and answers the ciphertext on standard output. The tool
     runs in its own process group, and the run, the read of its output included, is bounded by 60
     seconds; past it the group is killed, and it is killed again when the tool exits, so a
     descendant holding the output does not hold the command (one that left the group is caught by
     the bound). The size cap is checked before the status. A non-zero status is exit 1; a
     timeout, an I/O failure and an empty or longer-than-64-KiB answer are exit 3. The tool's
     standard error passes through, and the tool is killed if the provisioner dies;
   - `systemd-creds decrypt --name=<name> - -` must give back the generated key (the public keys
     are compared); a failure or a mismatch is exit 3 and nothing is linked;
   - the ciphertext is linked in with `journal::create_linked(dir, name, name.tmp, ciphertext,
     0600)`, which never replaces, after an exclusive lock on the directory so that two runs at once
     end as one exit 0 and one exit 5; a stale `name.tmp` is removed first, and also when the
     credential already exists (exit 5). A write that fails after the link was made is exit 3 with
     a message that the credential is in place but was not synced (check it and restart the unit);
     any other write failure says nothing is in place;
   - on success it prints `{"public_key_pem", "fingerprint", "created": true}` as `identity` does and
     says on standard error that a running unit must be restarted. It logs to standard error only.
   Exit statuses: 0 created, 1 it cannot, 2 usage, 3 an I/O failure (the message says whether the credential is in place), 5 exists. The
   logic is `identity::provision`; `main.rs` dispatches.
4. **The unit** carries `LoadCredentialEncrypted=nekoguruma-agent-key.device.v1:/etc/credstore.encrypted/nekoguruma-agent-key.device.v1`
   and no other credential directive. The name and the path are constants in `identity`, on every
   platform, so a test can check the unit against them. A unit whose credential is missing or does
   not decrypt fails before the agent starts and is restarted by the existing policy (the agent's
   exit 4 is for a decrypted key that is not a key); `RestartPreventExitStatus` is not changed.
5. **Selection**, as ADR-496 item 7 does for the user mode. A release build on Linux in device
   mode selects this store and reads `CREDENTIALS_DIRECTORY` alone, no `NGR_` variable. A debug
   build selects it only with `NGR_AGENT_KEY_STORE=systemd-credential` and device mode. An unset,
   empty or relative `CREDENTIALS_DIRECTORY` is no store (exit 1), with a message that tells the
   operator to start the command from its unit or with `systemd-run`. The value in user mode, or
   off Linux, is no store.
6. **Admin commands run under the unit's environment.** `ngr-agent identity` and `register` in
   device mode need the credential directory and the unit's account, so the documentation has the
   administrator run them with `systemd-run --wait --pipe -p User=ngr-agent -p Group=ngr-agent
   -p StateDirectory=nekoguruma -p StateDirectoryMode=0700 -p LoadCredentialEncrypted=...`
   (the documented order of installation: files, provision, issue the code, register under
   `systemd-run`, enable). `provision-key` prints the fingerprint the code is issued for.
7. **Tests.** The store is tested against a fake credential directory in a debug build; the
   command against a fake `systemd-creds` script first on `PATH` (it encrypts with base64, records
   its arguments and environment and misbehaves on request); the unit by a static check of its
   lines against the constants. Real systemd and a TPM are not part of CI.

The fifteen choices, in short: (1) a subcommand, not a flag of `identity`; (2) the directory
`/etc/credstore.encrypted` and an absolute `LoadCredentialEncrypted=` source; (3) exit 243 stays
restartable; (4) a closed `--with-key` list, default `auto`; (5) `--tpm2-pcrs=` empty, so no PCR
binding that a firmware update would break; (6) the credential variable alone selects, in a
release build; (7) the documented `systemd-run` route, with the fingerprint printed by provisioning;
(8) a regular file read without following links, and nothing more checked about it; (9) an empty
file is exit 4; (10) an unset variable is exit 1; (11) the debug value is explicit; (12) a fake tool
on `PATH` and a debug-only target override; (13) the key is verified before it is linked; (14)
standard-error logging only; (15) this ADR.

## Consequences

- A device that has not been provisioned has a unit that fails at start and restarts every 10
  seconds until the administrator provisions the key; the agent's own exit 1 for an empty
  credential directory shows only to a command started by hand. The exit status of a unit whose
  credential cannot be loaded is systemd's (recalled as 243, not checked here), and its
  restartability depends on that status not being in `RestartPreventExitStatus`.
- That `CREDENTIALS_DIRECTORY` over an empty directory reads as absent (exit 1) is built from the
  documented behaviour and tested against a fake directory, not on a real systemd.
- The TPM paths (`tpm2`, `host+tpm2`) pass through to the tool and are not tested in CI. Without PCR
  binding a TPM-sealed key survives a firmware update and also an attacker who boots other software
  on the same TPM; the TPM-held, non-exportable key of M6 (design 16.3) is the stronger answer.
- Provisioning needs systemd 250 or later (transient `LoadCredentialEncrypted=` for `systemd-run`);
  older hosts need a method of their own, still to be designed.
- Resetting the key is the administrator removing the credential file and provisioning again; the
  agent never replaces or deletes it, and it must then register again.
- A running unit does not see a key provisioned after it started; it is restarted.
- `provision-key` runs the `systemd-creds` found on the root account's `PATH`; a release build
  accepts only a root-owned file in root-owned directories that others cannot write.
- The tool dies with the provisioner (`PR_SET_PDEATHSIG`), but a descendant it started in its
  own process group is not killed when the provisioner is interrupted or terminated and may
  outlive it. The tool keeps a group of its own so that a timeout can kill what it started.
- Windows has no device-mode store yet.
