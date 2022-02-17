#![cfg(test)]

use std::process::Stdio;
use std::sync::Once;
use std::{sync::Arc, time::Duration};

use crate::acme::ca::{CACollector, CA};
use crate::acme::challenge::Challenger;
use crate::acme::handlers::{configure_routes, HandlerState, ServiceState};
use crate::acme::PostgresNonceValidator;
use crate::errors::db::MigrationError;
use crate::models::Postgres;
use crate::util::make_nonce;

use bollard::container::{LogsOptions, StartContainerOptions};
use openssl::error::ErrorStack;
use ratpack::app::TestApp;
use ratpack::prelude::*;

use bollard::{
    container::{Config, WaitContainerOptions},
    models::HostConfig,
    Docker,
};
use eggshell::EggShell;
use futures::TryStreamExt;
use lazy_static::lazy_static;
use openssl::sha::sha256;
use tempfile::{tempdir, TempDir};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

const DEBUG_VAR: &str = "DEBUG";

fn is_debug() -> bool {
    !std::env::var(DEBUG_VAR).unwrap_or_default().is_empty()
}

pub(crate) const DEFAULT_CONTACT: &str = "erik@hollensbe.org";

impl From<MigrationError> for eggshell::Error {
    fn from(me: MigrationError) -> Self {
        Self::Generic(me.to_string())
    }
}

#[derive(Clone)]
pub struct PGTest {
    gs: Arc<Mutex<EggShell>>,
    postgres: Postgres,
    docker: Arc<Mutex<Docker>>,
    // NOTE: the only reason we keep this is to ensure it lives the same lifetime as the PGTest
    // struct; otherwise the temporary directory is removed prematurely.
    temp: Arc<Mutex<TempDir>>,
}

fn pull_images(images: Vec<&str>) -> () {
    // bollard doesn't let you pull images. sadly, this is what I came up with until I can patch
    // it.

    for image in images {
        let mut cmd = &mut std::process::Command::new("docker");
        if !is_debug() {
            cmd = cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }

        let stat = cmd.args(vec!["pull", image]).status().unwrap();
        if !stat.success() {
            panic!("could not pull images");
        }
    }
}

async fn wait_for_images(images: Vec<&str>) -> () {
    let docker = Docker::connect_with_local_defaults().unwrap();

    for image in images {
        loop {
            match docker.inspect_image(image).await {
                Ok(_) => break,
                Err(_) => {
                    tokio::time::sleep(Duration::new(0, 200)).await;
                }
            }
        }
    }
}

static INIT: Once = Once::new();
lazy_static! {
    static ref IMAGES: Vec<&'static str> = vec![
        "certbot/certbot:latest",
        "postgres:latest",
        "zerotier/zlint:latest",
    ];
}

const HBA_CONFIG_PATH: &str = "hack/pg_hba.conf";

impl PGTest {
    pub async fn new(name: &str) -> Result<Self, eggshell::Error> {
        INIT.call_once(|| {
            let mut builder = &mut env_logger::builder();
            if is_debug() {
                builder = builder.filter_level(log::LevelFilter::Info)
            }
            builder.init();
            pull_images(IMAGES.to_vec());
        });

        wait_for_images(IMAGES.to_vec()).await;

        let pwd = std::env::current_dir().unwrap();
        let hbapath = pwd.join(HBA_CONFIG_PATH);

        let temp = tempdir().unwrap();

        let docker = Arc::new(Mutex::new(Docker::connect_with_local_defaults().unwrap()));
        let mut gs = EggShell::new(docker.clone()).await?;

        if is_debug() {
            gs.set_debug(true)
        }

        log::info!("launching postgres instance: {}", name);

        gs.launch(
            name,
            bollard::container::Config {
                image: Some("postgres:latest".to_string()),
                env: Some(
                    vec!["POSTGRES_PASSWORD=dummy", "POSTGRES_DB=coyote"]
                        .iter()
                        .map(|x| x.to_string())
                        .collect(),
                ),
                host_config: Some(HostConfig {
                    binds: Some(vec![
                        format!(
                            "{}:{}",
                            hbapath.to_string_lossy().to_string(),
                            "/etc/postgresql/pg_hba.conf"
                        ),
                        format!("{}:{}", temp.path().display(), "/var/run/postgresql"),
                    ]),
                    ..Default::default()
                }),
                cmd: Some(
                    vec![
                        "-c",
                        "shared_buffers=512MB",
                        "-c",
                        "max_connections=200",
                        "-c",
                        "unix_socket_permissions=0777",
                    ]
                    .iter()
                    .map(|x| x.to_string())
                    .collect(),
                ),
                ..Default::default()
            },
            None,
        )
        .await?;

        log::info!("waiting for postgres instance: {}", name);

        let mut postgres: Option<Postgres> = None;
        let config = format!("host={} dbname=coyote user=postgres", temp.path().display());

        while postgres.is_none() {
            let pg = Postgres::connect_one(&config).await;

            match pg {
                Ok(_) => postgres = Some(Postgres::new(&config, 200).await.unwrap()),
                Err(_) => tokio::time::sleep(Duration::new(1, 0)).await,
            }
        }

        log::info!("connected to postgres instance: {}", name);

        let postgres = postgres.unwrap();
        postgres.migrate().await?;

        Ok(Self {
            docker,
            gs: Arc::new(Mutex::new(gs)),
            postgres,
            temp: Arc::new(Mutex::new(temp)),
        })
    }

    pub fn db(&self) -> Postgres {
        self.postgres.clone()
    }

    pub fn eggshell(self) -> Arc<Mutex<EggShell>> {
        self.gs
    }

    pub fn docker(&self) -> Arc<Mutex<Docker>> {
        self.docker.clone()
    }
}
#[derive(Debug, Clone, Error)]
pub(crate) enum ContainerError {
    #[error("Unknown error encountered: {0}")]
    Generic(String),

    #[error("container failed with exit status: {0}: {1}")]
    Failed(i64, String),
}

fn short_hash(s: String) -> String {
    String::from(
        &sha256(s.as_bytes())
            .iter()
            .map(|c| format!("{:x}", c))
            .take(10)
            .collect::<Vec<String>>()
            .join("")[0..10],
    )
}

#[derive(Clone)]
pub(crate) struct TestService {
    pub pg: Box<PGTest>,
    pub nonce: PostgresNonceValidator,
    pub app: ratpack::app::TestApp<ServiceState, HandlerState>,
    pub url: String,
}

impl TestService {
    pub(crate) async fn new(name: &str) -> Self {
        let pg = PGTest::new(name).await.unwrap();
        let c = Challenger::new(Some(chrono::Duration::seconds(60)));
        let validator = PostgresNonceValidator::new(pg.db().clone());

        let c2 = c.clone();
        let pg2 = pg.db().clone();

        tokio::spawn(async move {
            loop {
                c2.tick(|_c| Some(())).await;
                c2.reconcile(pg2.clone()).await.unwrap();

                tokio::time::sleep(Duration::new(0, 250)).await;
            }
        });

        let ca = CACollector::new(Duration::new(0, 250));
        let mut ca2 = ca.clone();

        tokio::spawn(async move {
            let ca = CA::new_test_ca().unwrap();
            ca2.spawn_collector(|| -> Result<CA, ErrorStack> { Ok(ca.clone()) })
                .await
        });

        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = lis.local_addr().unwrap();
        let url = format!("http://{}", addr);
        drop(lis);

        let mut app = App::with_state(
            ServiceState::new(url.clone(), pg.db(), c, ca, validator.clone()).unwrap(),
        );

        configure_routes(&mut app, None);

        let a = app.clone();

        tokio::spawn(async move {
            a.serve(&addr.clone().to_string()).await.unwrap();
        });

        Self {
            pg: Box::new(pg),
            nonce: validator,
            app: TestApp::new(app),
            url,
        }
    }

    pub(crate) async fn zlint(&self, certs: Arc<TempDir>) -> Result<(), ContainerError> {
        log::info!("letsencrypt dir: {}", certs.path().display());
        let name = &format!("zlint-{}", short_hash(make_nonce(None)));

        let res = self
            .launch(
                name,
                Config {
                    attach_stdout: Some(is_debug()),
                    attach_stderr: Some(is_debug()),
                    image: Some("zerotier/zlint:latest".to_string()),
                    entrypoint: Some(
                        vec!["/bin/sh", "-c"]
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<String>>(),
                    ),
                    cmd: Some(vec![
                        "set -e; for file in /etc/letsencrypt/live/*/fullchain.pem; do zlint $file; done"
                            .to_string(),
                    ]),
                    host_config: Some(HostConfig {
                        binds: Some(vec![format!(
                            "{}:{}",
                            certs.path().to_string_lossy(),
                            "/etc/letsencrypt"
                        )]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await;

        if let Err(e) = res {
            return Err(ContainerError::Generic(e.to_string()));
        }

        self.wait(name).await?;
        return Ok(());
    }

    pub(crate) async fn certbot(
        &self,
        certs: Option<Arc<TempDir>>,
        command: String,
    ) -> Result<Arc<TempDir>, ContainerError> {
        let server_url = Url::parse(&self.url).unwrap();
        let server_url_hash = short_hash(server_url.to_string());
        let certs: Arc<tempfile::TempDir> = match certs {
            Some(certs) => certs,
            None => Arc::new(tempdir().unwrap()),
        };

        log::info!("letsencrypt dir: {}", certs.path().display());

        let name = &format!(
            "certbot-{}-{}",
            server_url_hash,
            short_hash(make_nonce(None))
        );

        let res = self
            .launch(
                name,
                Config {
                    image: Some("certbot/certbot:latest".to_string()),
                    entrypoint: Some(
                        vec!["/bin/sh", "-c"]
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<String>>(),
                    ),
                    cmd: Some(vec![format!(
                    // this 755 set is a hack around containers running as root and the
                    // test launching them running as a user.
                    "certbot --non-interactive --logs-dir '/etc/letsencrypt/logs' --server '{}' {} && chmod -R 755 /etc/letsencrypt",
                    server_url, command
                )]),
                    host_config: Some(HostConfig {
                        network_mode: Some("host".to_string()),
                        binds: Some(vec![format!(
                            "{}:{}",
                            certs.path().to_string_lossy(),
                            "/etc/letsencrypt"
                        )]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await;

        if let Err(e) = res {
            return Err(ContainerError::Generic(e.to_string()));
        }

        self.wait(name).await?;
        return Ok(certs);
    }

    async fn launch(
        &self,
        name: &str,
        config: Config<String>,
        start_opts: Option<StartContainerOptions<String>>,
    ) -> Result<(), eggshell::Error> {
        self.pg
            .clone()
            .eggshell()
            .lock()
            .await
            .set_debug(is_debug());

        self.pg
            .clone()
            .eggshell()
            .lock()
            .await
            .launch(name, config, start_opts)
            .await
    }

    async fn wait(&self, name: &str) -> Result<(), ContainerError> {
        loop {
            tokio::time::sleep(Duration::new(1, 0)).await;

            let locked = self.pg.docker.lock().await;
            let waitres = locked
                .wait_container::<String>(
                    name,
                    Some(WaitContainerOptions {
                        condition: "not-running".to_string(),
                    }),
                )
                .try_next()
                .await;

            if let Ok(Some(res)) = waitres {
                if res.status_code != 0 || res.error.is_some() {
                    let mut error = res.error.unwrap_or_default().message;

                    let logs = locked
                        .logs::<String>(
                            name,
                            Some(LogsOptions::<String> {
                                stderr: is_debug(),
                                stdout: is_debug(),
                                ..Default::default()
                            }),
                        )
                        .try_next()
                        .await;
                    if let Ok(Some(logs)) = logs {
                        error = Some(format!("{}", logs));
                        let logs = logs.into_bytes();
                        if logs.len() > 50 && is_debug() {
                            std::fs::write("error.log", logs).unwrap();
                            error = Some("error too long: error written to error.log".to_string())
                        }
                    }

                    return Err(ContainerError::Failed(
                        res.status_code,
                        error.unwrap_or_default(),
                    ));
                } else {
                    return Ok(());
                }
            }
        }
    }
}

mod tests {
    #[tokio::test(flavor = "multi_thread")]
    async fn pgtest_basic() {
        use super::PGTest;
        use spectral::prelude::*;

        let res = PGTest::new("pgtest_basic").await;
        assert_that!(res.is_ok()).is_true();
    }
}