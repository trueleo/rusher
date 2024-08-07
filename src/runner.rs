use crate::data::DatastoreModifier;
use crate::data::RuntimeDataStore;
use crate::{CRATE_NAME, SPAN_EXEC, SPAN_SCENARIO};

use crate::logical;

use async_scoped::{self, Scope};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tracing::{event, Instrument};

/// The Runner struct is the top level struct for managing and executing series of logical scenarios asynchronously.
pub struct Runner<'env> {
    logical: LogicalContext<'env>,
    #[cfg(feature = "tui")]
    enable_tui: bool,
    #[cfg(feature = "web")]
    enable_web: bool,
}

impl<'env> Runner<'env> {
    // Create new instance of Runner with a [Config](crate::config::Config) and list of [Scenario](create::logical::Scenario)
    pub fn new(scenarios: Vec<logical::Scenario<'env>>) -> Runner<'env> {
        Self {
            logical: LogicalContext { scenarios },
            #[cfg(feature = "tui")]
            enable_tui: false,
            #[cfg(feature = "web")]
            enable_web: false,
        }
    }

    // Spawn the runner
    pub async fn run(&self) -> Result<(), crate::error::Error> {
        #[cfg(feature = "tui")]
        let tui_handle = self.spawn_tui();

        #[cfg(feature = "web")]
        let web_handle = self.spawn_web();

        for (scenario_index, scenario) in self.scenarios().iter().enumerate() {
            let span = tracing::span!(target: CRATE_NAME, tracing::Level::INFO, SPAN_SCENARIO, name = scenario.label.as_ref(), id = scenario_index as u64);
            let _entered = span.enter();

            // create runtime datastores
            let mut runtime_context: Vec<_> = scenario
                .execution_provider
                .iter()
                .map(|_| ExecutionRuntimeCtx::new())
                .collect();

            // Init all the executors
            let mut executors = Vec::with_capacity(scenario.execution_provider.len());
            for (exec, ctx) in scenario
                .execution_provider
                .iter()
                .zip(runtime_context.iter_mut())
            {
                executors.push(exec.execution(ctx).await);
            }

            let mut scope =
                unsafe { async_scoped::Scope::create(async_scoped::spawner::use_tokio::Tokio) };

            // channel to gather user_results from every executor.
            let (user_result_tx, user_result_rx) = crate::channel();

            // trigger for spawning executors into scope.
            let mut spawn_executor: FuturesUnordered<_> = scenario
                .execution_provider
                .iter()
                .zip(executors.iter_mut())
                .enumerate()
                .map(|(idx, (logical, runtime))| async move {
                    tokio::time::sleep(logical.start_after()).await;
                    (idx, logical, runtime)
                })
                .collect();

            // spawn executors according to their delay
            while let Some((idx, logical, runtime)) = spawn_executor.next().await {
                let executor_name = logical.config().to_string();
                let span = tracing::span!(target: CRATE_NAME, parent: &span, tracing::Level::INFO, SPAN_EXEC, name = %executor_name, id = idx as u64);
                let task = runtime.execute(user_result_tx.clone());
                scope.spawn_cancellable(task.instrument(span.clone()), || ());
            }

            drop(spawn_executor);

            // drop last sender
            drop(user_result_tx);

            let has_user_terminated = has_user_terminated(user_result_rx).await;
            if has_user_terminated {
                scope.cancel();
            } else {
                Scope::collect(&mut scope).await;
            }

            drop(scope);

            futures::StreamExt::collect::<()>(futures::stream::FuturesUnordered::from_iter(
                executors.into_iter().map(|exec| exec.drop()),
            ))
            .await;

            if has_user_terminated {
                break;
            }
        }

        event!(name: "runner_exit", target: CRATE_NAME, tracing::Level::INFO, "Exit test");

        #[cfg(feature = "tui")]
        if let Some(handle) = tui_handle {
            let _ = handle.join();
        }

        #[cfg(feature = "web")]
        if let Some(handle) = web_handle {
            let _ = handle.await;
        }

        Ok(())
    }

    pub fn scenarios(&self) -> &[logical::Scenario<'env>] {
        &self.logical.scenarios
    }

    #[cfg(feature = "tui")]
    pub fn enable_tui(mut self, enable: bool) -> Self {
        self.enable_tui = enable;
        self
    }

    #[cfg(feature = "web")]
    pub fn enable_web(mut self, enable: bool) -> Self {
        self.enable_web = enable;
        self
    }

    #[cfg(feature = "tui")]
    fn spawn_tui(
        &self,
    ) -> Option<std::thread::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>> {
        use std::sync::{Arc, Mutex};

        if !self.enable_tui {
            return None;
        }

        let (tx, rx) = crate::channel();

        let tracer = crate::tracing::TracerLayer::new(tx);
        let subscriber = tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::Registry::default(),
            tracer,
        );

        tracing::subscriber::set_global_default(subscriber).unwrap();

        let app = Arc::new(Mutex::new(crate::app::App::new(&self.logical.scenarios)));
        Some(std::thread::spawn(|| crate::app::tui::run(app, rx)))
    }

    #[cfg(feature = "web")]
    fn spawn_web(
        &self,
    ) -> Option<tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>> {
        use std::sync::{Arc, Mutex};

        if !self.enable_web {
            return None;
        }

        let (tx, rx) = crate::channel();

        let tracer = crate::tracing::TracerLayer::new(tx);
        let subscriber = tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::Registry::default(),
            tracer,
        );

        tracing::subscriber::set_global_default(subscriber).unwrap();

        let app = Arc::new(Mutex::new(crate::app::App::new(&self.logical.scenarios)));
        Some(tokio::spawn(crate::app::web::run(app, rx)))
    }
}

struct LogicalContext<'env> {
    scenarios: Vec<logical::Scenario<'env>>,
}

#[derive(Debug, Default)]
pub(crate) struct ExecutionRuntimeCtx {
    datastore: RuntimeDataStore,
}

impl ExecutionRuntimeCtx {
    pub fn new() -> Self {
        Self {
            datastore: RuntimeDataStore::default(),
        }
    }

    pub async fn modify(&mut self, f: &dyn DatastoreModifier) {
        f.init_store(&mut self.datastore).await;
    }

    pub fn datastore_mut(&mut self) -> &mut RuntimeDataStore {
        &mut self.datastore
    }
}

async fn has_user_terminated<'s>(
    mut user_result_rx: tokio::sync::mpsc::UnboundedReceiver<Result<(), crate::error::Error>>,
) -> bool {
    let mut results = Vec::with_capacity(128);
    while user_result_rx.recv_many(&mut results, 128).await > 0 {
        if let Some(err) = results.iter().filter_map(|x| x.as_ref().err()).next() {
            event!(name: "termination_error", target: CRATE_NAME, tracing::Level::INFO, err = %err);
            return true;
        }
    }
    false
}
