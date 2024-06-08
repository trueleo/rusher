use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use futures::Future;
use tokio::sync::Mutex;
use tracing::{event, Instrument, Level};

use crate::{
    data::RuntimeDataStore,
    logical::{self, Rate},
    user::{AsyncUserBuilder, BoxedUser},
    UserResult, CRATE_NAME, SPAN_TASK, TARGET_USER_EVENT,
};

type ExecutorTask<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub trait Executor: Send {
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>);
}

pub(crate) enum DataExecutor<'a, Ub> {
    Once(Once<'a>),
    Constant(Constant<'a>),
    Shared(SharedIterations<'a>),
    PerUser(PerUserIteration<'a>),
    RampingUser(RampingUser<'a, Ub>),
    // ConstantArrivalRate is RampingArrivalRate with 1 stage
    ConstantArrivalRate(RampingArrivalRate<'a, Ub>),
    RampingArrivalRate(RampingArrivalRate<'a, Ub>),
}

impl<'a, Ub> DataExecutor<'a, Ub>
where
    Ub: AsyncUserBuilder + Sync,
{
    pub async fn new(
        datastore: &'a RuntimeDataStore,
        user_builder: &'a Ub,
        executor: logical::Executor,
    ) -> Self {
        match executor {
            logical::Executor::Once => {
                let mut users = build_users(datastore, user_builder, 1).await;
                Self::Once(Once::new(users.pop().unwrap()))
            }
            logical::Executor::Constant { users, duration } => {
                let users = build_users(datastore, user_builder, users).await;
                Self::Constant(Constant::new(users, duration))
            }
            logical::Executor::Shared {
                users,
                iterations,
                duration,
            } => {
                let users = build_users(datastore, user_builder, users).await;
                Self::Shared(SharedIterations::new(users, iterations, duration))
            }
            logical::Executor::PerUser { users, iterations } => {
                let users = build_users(datastore, user_builder, users).await;
                Self::PerUser(PerUserIteration::new(users, iterations))
            }
            logical::Executor::ConstantArrivalRate {
                pre_allocate_users,
                rate,
                max_users,
                duration,
            } => Self::ConstantArrivalRate(RampingArrivalRate::new(
                datastore,
                user_builder,
                pre_allocate_users,
                vec![(rate, duration)],
                max_users,
            )),
            logical::Executor::RampingUser {
                pre_allocate_users,
                stages,
            } => Self::RampingUser(RampingUser::new(
                datastore,
                user_builder,
                stages,
                pre_allocate_users,
            )),
            logical::Executor::RampingArrivalRate {
                pre_allocate_users,
                max_users,
                stages,
            } => Self::RampingArrivalRate(RampingArrivalRate::new(
                datastore,
                user_builder,
                pre_allocate_users,
                stages,
                max_users,
            )),
        }
    }
}

#[async_trait::async_trait]
impl<'a, Ub> Executor for DataExecutor<'a, Ub>
where
    Ub: AsyncUserBuilder + Sync,
{
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>) {
        match self {
            DataExecutor::Once(exec) => exec.execute(),
            DataExecutor::Constant(exec) => exec.execute(),
            DataExecutor::Shared(exec) => exec.execute(),
            DataExecutor::PerUser(exec) => exec.execute(),
            DataExecutor::RampingUser(exec) => exec.execute(),
            DataExecutor::ConstantArrivalRate(exec) => exec.execute(),
            DataExecutor::RampingArrivalRate(exec) => exec.execute(),
        }
    }
}

pub(crate) struct Once<'a> {
    user: BoxedUser<'a>,
}

impl<'a> Once<'a> {
    fn new(user: BoxedUser<'a>) -> Self {
        Once { user }
    }
}

impl<'a> Executor for Once<'a> {
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>) {
        let (tx, rx) = crate::channel();
        let task = self.user.call();
        let exec = async move {
            let spawner = async_scoped::spawner::use_tokio::Tokio;
            let mut scope = unsafe { async_scoped::TokioScope::create(spawner) };
            event!(target: CRATE_NAME, Level::INFO, users = 1u64, users_max = 1u64);
            scope.spawn_cancellable(
                async move {
                    let _ = tx.send(user_call(task).await);
                }
                .instrument(tracing::span!(target: CRATE_NAME, tracing::Level::INFO, SPAN_TASK)),
                || (),
            );
            let _ = scope.collect().await;
        };
        (Box::pin(exec), rx)
    }
}

pub(crate) struct Constant<'a> {
    users: Vec<BoxedUser<'a>>,
    duration: Duration,
}

impl<'a> Constant<'a> {
    fn new(users: Vec<BoxedUser<'a>>, duration: Duration) -> Self {
        Self { users, duration }
    }
}

impl<'a> Executor for Constant<'a> {
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>) {
        let (tx, rx) = crate::channel();

        let users_len = self.users.len();
        let total_duration_as_secs = self.duration.as_secs();
        let total_duration = self.duration;

        let end_time = Instant::now() + total_duration;
        let tasks = self.users.iter_mut().map(move |user| {
            let tx = tx.clone();
            async move {
                while std::time::Instant::now() < end_time {
                    let res = user_call(user.call())
                        .instrument(
                            tracing::span!(target: CRATE_NAME, tracing::Level::INFO, SPAN_TASK),
                        )
                        .await;
                    let _ = tx.send(res);
                }
            }
        });

        let task = async move {
            event!(target: CRATE_NAME, Level::INFO, users = users_len, users_max = users_len);
            event!(target: CRATE_NAME, Level::INFO, total_duration = total_duration_as_secs);
            let spawner = async_scoped::spawner::use_tokio::Tokio;
            let mut scope = unsafe { async_scoped::TokioScope::create(spawner) };
            for task in tasks {
                scope.spawn_cancellable(task.in_current_span(), || ());
            }
            let _ = scope.collect().await;
        };

        (Box::pin(task), rx)
    }
}

pub(crate) struct SharedIterations<'a> {
    users: Vec<BoxedUser<'a>>,
    iterations: usize,
    duration: Duration,
}

impl<'a> SharedIterations<'a> {
    fn new(users: Vec<BoxedUser<'a>>, iterations: usize, duration: Duration) -> Self {
        Self {
            users,
            iterations,
            duration,
        }
    }
}

impl<'a> SharedIterations<'a> {
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>) {
        let (tx, rx) = crate::channel();
        let users_len = self.users.len();
        let iterations = self.iterations;
        let total_duration_as_secs = self.duration.as_secs();

        let end_time = Instant::now() + self.duration;

        let task = async move {
            event!(target: CRATE_NAME, Level::INFO, users = users_len, users_max = users_len);
            event!(target: CRATE_NAME, Level::INFO, total_duration = total_duration_as_secs);
            let iterations_completed = AtomicUsize::new(iterations);
            let tasks = self.users.iter_mut().map(|user| {
                let tx = tx.clone();
                let iterations_completed = &iterations_completed;
                async move {
                    while std::time::Instant::now() < end_time {
                        let current_iteration =
                            iterations_completed.fetch_add(1, Ordering::Relaxed);
                        if current_iteration >= iterations {
                            break;
                        }
                        let _ = tx.send(user_call(user.call()).instrument(
                            tracing::span!(target: CRATE_NAME, tracing::Level::INFO, SPAN_TASK),
                        ).await);
                    }
                }
            });

            let spawner = async_scoped::spawner::use_tokio::Tokio;
            let mut scope = unsafe { async_scoped::TokioScope::create(spawner) };
            for task in tasks {
                scope.spawn_cancellable(task.in_current_span(), || ());
            }
            let _ = scope.collect().await;
        };

        (Box::pin(task), rx)
    }
}

pub(crate) struct PerUserIteration<'a> {
    users: Vec<BoxedUser<'a>>,
    iterations: usize,
}

impl<'a> PerUserIteration<'a> {
    fn new(users: Vec<BoxedUser<'a>>, iterations: usize) -> Self {
        Self { users, iterations }
    }
}

impl<'a> Executor for PerUserIteration<'a> {
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>) {
        let (tx, rx) = crate::channel();
        let Self { users, iterations } = self;
        let users_len = users.len();
        let iterations = *iterations;
        let tasks = users.iter_mut().map(move |user| {
            let tx = tx.clone();
            async move {
                for _ in 0..iterations {
                    let _ = tx.send(
                        user_call(user.call())
                            .instrument(
                                tracing::span!(target: CRATE_NAME, tracing::Level::INFO, SPAN_TASK),
                            )
                            .await,
                    );
                }
            }
        });

        let task = async move {
            event!(target: CRATE_NAME, Level::INFO, users = users_len, users_max = users_len);
            event!(target: CRATE_NAME, Level::INFO, total_iteration = iterations);
            let spawner = async_scoped::spawner::use_tokio::Tokio;
            let mut scope = unsafe { async_scoped::TokioScope::create(spawner) };
            for task in tasks {
                scope.spawn_cancellable(task.in_current_span(), || ());
            }
            let _ = scope.collect().await;
        };

        (Box::pin(task), rx)
    }
}

pub(crate) struct RampingUser<'a, Ub> {
    datastore: &'a RuntimeDataStore,
    user_builder: &'a Ub,
    pre_allocate_users: usize,
    stages: Vec<(Duration, usize)>,
}

impl<'a, Ub> RampingUser<'a, Ub> {
    fn new(
        datastore: &'a RuntimeDataStore,
        user_builder: &'a Ub,
        stages: Vec<(Duration, usize)>,
        initial_users: usize,
    ) -> Self {
        Self {
            datastore,
            user_builder,
            pre_allocate_users: initial_users,
            stages,
        }
    }
}

impl<'a, Ub> Executor for RampingUser<'a, Ub>
where
    Ub: AsyncUserBuilder,
{
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>) {
        let (tx, rx) = crate::channel();
        let datastore = self.datastore;
        let user_builder = self.user_builder;
        let pre_allocated_users = self.pre_allocate_users;
        let stages = &*self.stages;
        let total_duration: u64 = stages.iter().map(|(duration, _)| duration.as_secs()).sum();

        let task = async move {
            event!(target: CRATE_NAME, Level::INFO, total_duration = total_duration);
            let mut users = build_users(datastore, user_builder, pre_allocated_users).await;
            event!(target: CRATE_NAME, Level::INFO, users = users.len(), users_max = pre_allocated_users);

            for (index, (duration, target_users)) in stages.iter().enumerate() {
                event!(target: CRATE_NAME, Level::INFO, stage = index + 1, stages = stages.len(), stage_duration = duration.as_secs());
                event!(target: CRATE_NAME, Level::INFO, users = users.len(), users_max = target_users.max(&pre_allocated_users));

                let len = users.len();
                if len < *target_users {
                    users.extend(build_users(datastore, user_builder, target_users - len).await);
                }
                event!(target: CRATE_NAME, Level::INFO, users = users.len(), users_max = target_users.max(&pre_allocated_users));

                let end_time = Instant::now() + *duration;
                let tasks = users.iter_mut().map(|user| {
                    let tx = tx.clone();
                    async move {
                        while Instant::now() < end_time {
                            let _ = tx.send(user_call(user.call()).instrument(tracing::span!(target: CRATE_NAME, tracing::Level::INFO, SPAN_TASK)).await);
                        }
                    }
                });
                let spawner = async_scoped::spawner::use_tokio::Tokio;
                let mut scope = unsafe { async_scoped::TokioScope::create(spawner) };
                tasks.into_iter().for_each(|task| {
                    scope.spawn_cancellable(task.in_current_span(), || ());
                });
                let _ = scope.collect().await;
            }
        };

        (Box::pin(task), rx)
    }
}

pub(crate) struct RampingArrivalRate<'a, Ub> {
    datastore: &'a RuntimeDataStore,
    user_builder: &'a Ub,
    pre_allocate_users: usize,
    stages: Vec<(Rate, Duration)>,
    max_users: usize,
}

impl<'a, Ub> RampingArrivalRate<'a, Ub> {
    fn new(
        datastore: &'a RuntimeDataStore,
        user_builder: &'a Ub,
        pre_allocate_users: usize,
        stages: Vec<(Rate, Duration)>,
        max_users: usize,
    ) -> Self {
        Self {
            datastore,
            user_builder,
            pre_allocate_users,
            stages,
            max_users,
        }
    }
}

impl<'a, Ub> Executor for RampingArrivalRate<'a, Ub>
where
    Ub: AsyncUserBuilder,
{
    fn execute(&mut self) -> (ExecutorTask<'_>, crate::Receiver<UserResult>) {
        let (tx, rx) = crate::channel();

        let datastore = self.datastore;
        let user_builder = self.user_builder;
        let pre_allocated_users = self.pre_allocate_users;
        let max_users = self.max_users;
        let stages = &*self.stages;
        let total_duration: u64 = stages.iter().map(|(_, duration)| duration.as_secs()).sum();

        let task = async move {
            event!(target: CRATE_NAME, Level::INFO, total_duration = total_duration);
            let mut users: Vec<_> = build_users(datastore, user_builder, pre_allocated_users)
                .await
                .into_iter()
                .map(Mutex::new)
                .collect();
            event!(target: CRATE_NAME, Level::INFO, users = users.len(), users_max = pre_allocated_users);

            for (index, (Rate(rate, time_unit), duration)) in stages.iter().enumerate() {
                let end_time = Instant::now() + *duration;
                event!(target: CRATE_NAME, Level::INFO, stage = index + 1, stages = stages.len(), stage_duration = duration.as_secs());

                while Instant::now() < end_time {
                    let next_rate_check_time = Instant::now() + *time_unit;
                    let mut current_rate = 0;

                    let spawner = async_scoped::spawner::use_tokio::Tokio;
                    let mut scope = unsafe { async_scoped::TokioScope::create(spawner) };

                    let mut user_iter = users.iter().cycle().filter_map(|x| x.try_lock().ok());

                    let now = Instant::now();
                    while now < next_rate_check_time && now < end_time && current_rate < *rate {
                        let mut user = user_iter.next().unwrap();
                        let tx = tx.clone();
                        let task = async move {
                            let _ = tx.send(user_call(user.call()).await);
                        };
                        let span =
                            tracing::span!(target: CRATE_NAME, tracing::Level::INFO, SPAN_TASK);
                        scope.spawn_cancellable(task.instrument(span), || ());
                        current_rate += 1;
                    }

                    scope.collect().await;
                    drop(scope);

                    if current_rate < *rate && users.len() < max_users {
                        users.extend(
                            build_users(datastore, user_builder, rate - current_rate)
                                .await
                                .into_iter()
                                .map(Mutex::new),
                        );
                    }
                    event!(target: CRATE_NAME, Level::INFO, users = users.len(), users_max = pre_allocated_users);

                    if Instant::now() <= end_time || current_rate < *rate {
                        // Sleep until to make sure we wait before next set of task;
                        tokio::time::sleep_until(next_rate_check_time.into()).await;
                    }
                }
            }
        };

        (Box::pin(task), rx)
    }
}

async fn user_call<'a>(
    task: Pin<Box<dyn Future<Output = Result<(), crate::error::Error>> + Send + 'a>>,
) -> Result<(), crate::error::Error> {
    let res = task.await;
    if let Err(ref err) = res {
        event!(name: "error", target: TARGET_USER_EVENT, Level::INFO, err = %err)
    }
    res
}
async fn build_users<'a, Ub>(
    runtime: &'a RuntimeDataStore,
    user_builder: &'a Ub,
    count: usize,
) -> Vec<BoxedUser<'a>>
where
    Ub: AsyncUserBuilder,
{
    let mut res = vec![];
    for _ in 0..count {
        let user = user_builder.build(runtime).await.unwrap();
        res.push(user)
    }
    res
}
