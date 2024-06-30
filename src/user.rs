use futures::Future;

use crate::{data::RuntimeDataStore, error::Error, UserResult};

/// The `User` trait defines the fundamental component of this library.
/// A `User` represents a state coupled with an asynchronous function that can be executed asynchronously.
/// This is the primary trait that any user of this library will implement for their test cases.
/// This can be thought of as a virtual user such that, The executor will only run the async function defined by the user only once at a time.
/// This allows user state to be mutated by the execution but the downside is that enough users need to be generated by executor to generate sufficient load in certain scenarios.
///
///
/// ### User Trait Bounds
///
/// - `User` is `Send`, which allows `&mut self` to be used across thread boundaries.
///   For more information, see the [Rust documentation on the Send trait](https://doc.rust-lang.org/std/marker/trait.Send.html).
///
/// ### Note
/// A concrete implementation of the `User` trait can capture arguments and reference data from higher layers,
/// such as the [RuntimeDataStore](crate::data::RuntimeDataStore) defined in the scenario or in its executor.

pub trait User: Send {
    fn call(&mut self) -> impl std::future::Future<Output = UserResult> + std::marker::Send;
}

impl<F, Fut> User for F
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = UserResult> + Send,
{
    async fn call(&mut self) -> UserResult {
        self().await
    }
}

/// Builds a user instance asynchronously.
/// The type implementing this should also implement Sync as this is shared across runtime executors.
/// Runtime executors given the type and configuration can request more user in middle of execution.  
///
/// ### Generic types and their constraints
///
/// - `U` must be a User type and must have a lifetime bound of datastore `'a`.
#[async_trait::async_trait]
pub trait AsyncUserBuilder<'a>: Sync {
    type Output: User + 'a;
    /// Build a new instance of user
    async fn build(&self, store: &'a RuntimeDataStore) -> Result<Self::Output, Error>;
}

#[async_trait::async_trait]
impl<'a, F> AsyncUserBuilder<'a> for F
where
    F: async_fn_traits::AsyncFn1<&'a RuntimeDataStore> + Sync,
    <F as async_fn_traits::AsyncFn1<&'a RuntimeDataStore>>::Output: User + 'a,
    for<'b> <F as async_fn_traits::AsyncFn1<&'b RuntimeDataStore>>::OutputFuture: Send,
{
    type Output = <F as async_fn_traits::AsyncFn1<&'a RuntimeDataStore>>::Output;

    async fn build(&self, store: &'a RuntimeDataStore) -> Result<Self::Output, Error> {
        Ok((self)(store).await)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        data::RuntimeDataStore,
        user::{AsyncUserBuilder, User},
        UserResult,
    };

    #[allow(dead_code)]
    struct BorrowUser<'a> {
        s: &'a str,
    }

    impl<'a> User for BorrowUser<'a> {
        async fn call(&mut self) -> UserResult {
            Ok(())
        }
    }

    #[test]
    fn test_lifetimes() {
        let mut store = RuntimeDataStore::default();
        store.insert("A".to_string());

        async fn user_builder(r: &RuntimeDataStore) -> BorrowUser<'_> {
            let s: &String = r.get().unwrap();
            BorrowUser { s: s.as_str() }
        }

        let _ = futures::executor::block_on(AsyncUserBuilder::build(&user_builder, &store));
    }
}
