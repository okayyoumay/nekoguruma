//! `ngr-agent debug fetch` against a real local-profile server and the bundled RustFS (design
//! 14.3, ADR-497): a registered agent asks the download route for a document its job's signed
//! instruction names, and the file it caches is byte-identical to the object that was uploaded.
//! The document is stored through the server's own route (`PUT /api/v1/documents/{sha256}`,
//! ADR-500) by a worker of the agent's tenant; the job row holds an instruction signed by the
//! server's key, which the agent verifies with the key its record holds.
//!
//! A release agent has no plain-file key store and no debug command, so this module is compiled
//! in debug builds only. The test needs `NGR_TEST_RUSTFS_BIN` (`scripts/fetch-rustfs.sh`).

use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use server::Server;
use server::config::Http;
use server::storage::{MultipartStore, S3Store, S3Tuning};
use server::uploads::Backend;
use shared_proto::{AgentId, INSTRUCTION_VERSION, JobId, JobInstruction, JobKind, VciId};

use crate::agent_register::{AgentEnv, fingerprint, run, text};
use crate::documents::{Operator, document_of, operator_at, put_document};
use crate::redeem::World;
use crate::serving::Running;
use crate::storage::sha256;
use crate::uploads::{hex, v4};

/// A registered agent, a server over the real object server, and what is needed to fetch.
struct Fixture {
    world: World,
    server: Running,
    real: server::object_server::ObjectServer,
    /// Owns the object server's data directory, which must outlive it.
    _object: crate::object_server_rustfs::Real,
    env: AgentEnv,
    agent_id: String,
    /// A worker of the agent's tenant with an empty scope, who stores the documents.
    worker: Operator,
}

impl Fixture {
    async fn new() -> Fixture {
        let world = World::new().await;
        world.add_operator("alice").await;
        let mut object = crate::object_server_rustfs::Real::new();
        let real = object.start().await;
        let store = Arc::new(
            tokio::time::timeout(
                Duration::from_secs(60),
                S3Store::open(
                    real.port(),
                    real.credentials(),
                    "nekoguruma",
                    S3Tuning::default(),
                ),
            )
            .await
            .expect("open must not hang")
            .unwrap(),
        );
        let backend = Backend::new(Some(Arc::clone(&store) as Arc<dyn MultipartStore>));
        let http = Http {
            port: 0,
            shutdown_grace_secs: 2,
        };
        let server = Server::bind_with_store(&http, world.pg.pool(), world.key.clone(), backend)
            .await
            .unwrap();
        let sessions = server.operator_sessions();
        let server = Running::spawn(server);
        let env = AgentEnv::new();
        let fingerprint = fingerprint(&env).await;
        let request =
            server::registration::IssueRequest::new("alice", "Bench PC", &fingerprint, None)
                .unwrap();
        let issued = server::registration::issue(&world.pool, &request)
            .await
            .unwrap();
        env.install(server.addr.port());
        let args: Vec<String> = [
            "register",
            "--mode",
            "user",
            "--server-key",
            &issued.server_key_id,
        ]
        .map(str::to_owned)
        .to_vec();
        let output = run(env.command(&args), issued.code.as_str().to_owned()).await;
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let agent_id = world.sql("SELECT id::text FROM agents").await;
        let tenant = world.sql("SELECT tenant_id::text FROM agents").await;
        let worker = operator_at(
            &world.pool,
            &sessions,
            &tenant,
            "wanda",
            &[("worker", "{}")],
        )
        .await;
        Fixture {
            world,
            server,
            real,
            _object: object,
            env,
            agent_id,
            worker,
        }
    }

    async fn finish(self) {
        self.server.finish().await;
        self.real.stop(std::future::pending()).await;
        self.world.finish().await;
    }

    /// Stores `bytes` through the document route: the store reads the object back, and the
    /// agent's tenant gets its row.
    async fn store_document(&self, bytes: &[u8]) {
        let r = put_document(self.server.addr, &self.worker.cookie, bytes).await;
        assert_eq!(r.status, 201, "{} {}", r.head, r.body);
    }

    /// A job of the agent's owner naming `documents`, signed by the server's key; returns the
    /// envelope's file in the agent's home.
    async fn job_naming(&self, documents: Vec<[u8; 32]>) -> PathBuf {
        let owner: String =
            sqlx::query_scalar("SELECT owner_id::text FROM agents WHERE id = $1::uuid")
                .bind(&self.agent_id)
                .fetch_one(&self.world.pool)
                .await
                .unwrap();
        let id = v4();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let instruction = JobInstruction {
            version: INSTRUCTION_VERSION,
            job_id: JobId(id.clone()),
            ownership_generation: 1,
            agent_id: AgentId(self.agent_id.clone()),
            operator_id: owner.clone(),
            kind: JobKind::RunSequence,
            vci: VciId("vci-a".to_owned()),
            vin: None,
            document_hashes: documents,
            artifact_hash: None,
            plan_hash: None,
            issued_at: now,
            start_deadline: now + 3_600,
        };
        let envelope = self.world.key.seal(&instruction).await.unwrap();
        sqlx::query(
            "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
                 confirmation_level, start_deadline, signed_job, signing_key_id, \
                 idempotency_key) \
             SELECT a.tenant_id, $2::uuid, 'runSequence', a.id, o.id, 'vci-a', 'notifyOnly', \
                 now() + interval '1 hour', $3, $4, $2 \
             FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id \
             WHERE a.id = $1::uuid AND o.id = $5::uuid",
        )
        .bind(&self.agent_id)
        .bind(&id)
        .bind(&envelope)
        .bind(self.world.key.key_id())
        .bind(&owner)
        .execute(&self.world.pool)
        .await
        .unwrap();
        let file = self.env.home.path().join(format!("{id}.instruction"));
        std::fs::write(&file, &envelope).unwrap();
        file
    }

    async fn fetch(&self, instruction: &std::path::Path, digest: &[u8; 32]) -> Output {
        let args: Vec<String> = [
            "debug",
            "fetch",
            "--mode",
            "user",
            "--instruction",
            instruction.to_str().unwrap(),
            "--kind",
            "document",
            "--sha256",
            &hex(digest),
            "--attempts",
            "1",
        ]
        .map(str::to_owned)
        .to_vec();
        run(self.env.command(&args), String::new()).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_fetches_a_stored_document_byte_identical_and_a_revoked_agent_is_refused() {
    let fx = Fixture::new().await;
    let bytes = document_of(300_000);
    let digest = sha256(&bytes);
    fx.store_document(&bytes).await;
    let job = fx.job_naming(vec![digest]).await;

    let output = fx.fetch(&job, &digest).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed["length"], bytes.len());
    let path = PathBuf::from(printed["path"].as_str().unwrap());
    assert_eq!(
        path,
        fx.env
            .state
            .path()
            .join("objects")
            .join("documents")
            .join(hex(&digest))
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    // Nothing is left in the working area.
    let tmp = fx.env.state.path().join("objects").join("tmp");
    assert_eq!(std::fs::read_dir(tmp).unwrap().count(), 0);

    // A digest the instruction does not name is refused without a request (exit 1).
    let other = sha256(b"not named");
    let output = fx.fetch(&job, &other).await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));

    // A revoked agent: with the cache emptied, the exchange is refused and the command fails.
    std::fs::remove_file(&path).unwrap();
    sqlx::query("UPDATE agents SET revoked_at = now() WHERE id = $1::uuid")
        .bind(&fx.agent_id)
        .execute(&fx.world.pool)
        .await
        .unwrap();
    let output = fx.fetch(&job, &digest).await;
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert!(!path.exists());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("retry"), "{stderr}");
    fx.finish().await;
}
