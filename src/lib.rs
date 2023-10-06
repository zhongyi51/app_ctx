mod error;

use crate::error::{AppContextDroppedError, BeanError};
use async_trait::async_trait;
use once_cell::sync::OnceCell;
use std::any::{type_name, Any};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display};
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// metadata of a bean. it can be used as map key
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct BeanMetadata {
    /// typename of the bean, such as `XXXService`
    type_name: &'static str,
    /// an identifier for the bean, to distinguish different
    /// beans with same type name
    bean_name: &'static str,
}

impl BeanMetadata {
    pub(crate) fn build_meta<T>(name: &'static str) -> BeanMetadata {
        BeanMetadata {
            type_name: type_name::<T>(),
            bean_name: name,
        }
    }
}

/// A trait for beans which can be created from `AppContextBuilder`.
/// `ctx` is the AppContext for acquiring lazy-initialized beans,
/// and `extras` are extra params for this build.
pub trait BuildFromContext<E, CtxErr = (), InitErr = ()> {
    /// build the beans from
    fn build_from(ctx: &AppContextBuilder, extras: E) -> Result<Self, CtxErr>
    where
        Self: Sized;

    /// initialization method after all beans have been built
    /// because of potential cyclic invocations during the initialization, only
    /// immutable references are allowed
    fn init_self(&self) -> Result<(), InitErr> {
        return Ok(());
    }
}

/// async implementation for building context
#[async_trait::async_trait]
pub trait BuildFromContextAsync<E, CtxErr = (), InitErr = ()> {
    async fn build_from(ctx: &AppContextBuilder, extras: E) -> Result<Self, CtxErr>
    where
        Self: Sized;

    /// initialization method after all beans have been built
    /// because of potential cyclic invocations during the initialization, only
    /// immutable references are allowed
    async fn init_self(&self) -> Result<(), InitErr> {
        return Ok(());
    }
}

/// like `Class<T>` in java
pub struct BeanType<T>(PhantomData<T>);

pub trait BeanTypeOf<T> {
    const BEAN_TYPE: BeanType<T>;
}

impl<T> BeanTypeOf<T> for T {
    const BEAN_TYPE: BeanType<T> = BeanType(PhantomData);
}

/// make the usage easier
pub struct RefWrapper<T>(Arc<OnceCell<T>>);

impl<T> Deref for RefWrapper<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.get().expect("bean is not initialized properly")
    }
}

/// the weak reference of bean, avoiding circular references
pub struct BeanRef<T> {
    inner: Weak<OnceCell<T>>,
}

impl<T> BeanRef<T> {
    /// acquire the bean, if corresponding app context is dropped, `None`
    /// will be returned
    pub fn try_acquire(&self) -> Result<RefWrapper<T>, AppContextDroppedError> {
        self.inner
            .upgrade()
            .map(|c| RefWrapper(c))
            .ok_or(AppContextDroppedError)
    }

    /// acquire the bean, if corresponding app context is dropped,
    /// a panic will be thrown
    pub fn acquire(&self) -> RefWrapper<T> {
        self.try_acquire()
            .expect("app context is dropped, all beans are not acquirable")
    }

    /// check whether the `AppContext` related to this bean is dropped
    pub fn is_active(&self) -> bool {
        self.try_acquire().is_ok()
    }
}

/// a wrapper for `Arc<dyn Any +Send +Sync>`, to store helper data
pub struct BeanWrapper {
    bean: Arc<dyn Any + Send + Sync>,
    initialized: AtomicBool,
    meta: BeanMetadata,
}

impl Clone for BeanWrapper {
    fn clone(&self) -> Self {
        Self {
            bean: self.bean.clone(),
            initialized: AtomicBool::new(self.initialized.load(Ordering::Acquire)),
            meta: self.meta.clone(),
        }
    }
}

impl BeanWrapper {
    /// wrap a `OnceCell` and meta, to erase its type
    pub(crate) fn wrap<T>(bean: OnceCell<T>, meta: BeanMetadata) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            initialized: AtomicBool::new(bean.get().is_some()),
            bean: Arc::new(bean),
            meta,
        }
    }

    /// because we do not know its type, we cannot downcast
    /// it into OnceCell, so we need to use an extra field
    pub(crate) fn initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// check whether this bean needs to be initialized in async context
    pub(crate) fn is_async_bean(&self) -> bool {
        return false;
    }

    /// build `BeanRef<T>` from inner arc
    pub(crate) fn build_bean_ref<T>(&self) -> BeanRef<T>
    where
        T: Send + Sync + 'static,
    {
        let weak_arc = self
            .bean
            .clone()
            .downcast::<OnceCell<T>>()
            .ok()
            .map(|c| Arc::downgrade(&c))
            .expect("bean type is not matched");
        BeanRef { inner: weak_arc }
    }

    /// wrap the bean into it
    pub(crate) fn set_inner<T>(&self, bean: T)
    where
        T: Send + Sync + 'static,
    {
        self.bean
            .clone()
            .downcast::<OnceCell<T>>()
            .ok()
            .map(|c| c.set(bean).ok())
            .flatten()
            .expect("bean is setted before");
        self.initialized.store(true, Ordering::Release);
    }
}

/// the inner storage for beans
/// the `Arc<...>` stores beans with lazy initialization
pub struct AppContextInner {
    bean_map: HashMap<BeanMetadata, BeanWrapper>,
}

/// the context to store all beans
/// `AppContext` owns the ownership of all registered beans
/// When `AppContext` is dropped, all beans will be dropped too
pub struct AppContextBuilder {
    inner: Mutex<AppContextInner>,
    init_fn_map: HashMap<BeanMetadata, InitFnEnum>,
}

impl AppContextBuilder {
    /// init method
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AppContextInner {
                bean_map: Default::default(),
            }),
            init_fn_map: Default::default(),
        }
    }

    /// acquire the `BeanRef` of a bean
    /// because of the initialization order, returned `BeanRef` may not be initialized
    /// the method `acquire_bean_or_init` only requires immutable reference, so
    /// the beans which implements `BuildFromContext` can invoke it during the construction
    pub fn acquire_bean_or_init<T>(&self, _ty: BeanType<T>, name: &'static str) -> BeanRef<T>
    where
        T: Send + Sync + 'static,
    {
        let meta = BeanMetadata::build_meta::<T>(name);

        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::wrap(OnceCell::<T>::new(), meta))
            .build_bean_ref()
    }

    /// construct a bean and hand over to `AppContextBuilder`
    /// the bean type must implement `BuildFromContext`
    pub fn construct_bean<T, E, Err, Err2>(
        mut self,
        _ty: BeanType<T>,
        name: &'static str,
        extras: E,
    ) -> Result<Self, Err>
    where
        T: Send + Sync + BuildFromContext<E, Err, Err2> + 'static,
        Err2: Send + Sync + 'static,
    {
        let meta = BeanMetadata::build_meta::<T>(name);
        let bean = T::build_from(&self, extras)?;
        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::wrap(OnceCell::<T>::new(), meta.clone()))
            .set_inner(bean);
        self.init_fn_map
            .insert(meta.clone(), build_init_fn::<E, Err, Err2, T>());
        Ok(self)
    }

    /// construct a bean and hand over to `AppContextBuilder`
    /// the bean type must implement `BuildFromContextAsync`
    pub async fn construct_bean_async<T, E, Err, Err2>(
        mut self,
        _ty: BeanType<T>,
        name: &'static str,
        extras: E,
    ) -> Result<Self, Err>
    where
        T: Send + Sync + BuildFromContextAsync<E, Err, Err2> + 'static,
        Err2: Send + Sync + 'static,
    {
        let meta = BeanMetadata::build_meta::<T>(name);
        let bean = T::build_from(&self, extras).await?;
        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::wrap(OnceCell::<T>::new(), meta.clone()))
            .set_inner(bean);
        self.init_fn_map
            .insert(meta.clone(), build_init_fn_async::<E, Err, Err2, T>());
        Ok(self)
    }

    /// finish construction and create `AppContext` without Mutex
    /// this method will go over all beans and ensure all beans are initialized
    /// but the initialization method will not be ran
    pub fn build_without_init(self) -> Result<AppContext, BeanError> {
        if let Some((uninit_meta, _)) = self
            .inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .iter()
            .find(|(meta, bean)| !bean.initialized())
        {
            return Err(BeanError::NotInitialized(uninit_meta.clone()));
        }
        Ok(AppContext {
            inner: Arc::new(self.inner.into_inner().expect("unexpected lock")),
        })
    }

    pub fn build_non_async(self) -> Result<AppContext, BeanError> {
        {
            let wrapper = self.inner.lock().expect("unexpected lock");
            if let Some((uninit_meta, _)) = wrapper
                .bean_map
                .iter()
                .find(|(meta, bean)| !bean.initialized())
            {
                return Err(BeanError::NotInitialized(uninit_meta.clone()));
            }
            for (k, v) in self.init_fn_map {
                let wrapper_cloned = wrapper
                    .bean_map
                    .get(&k)
                    .expect("unexpected meta key error")
                    .clone();
                if let InitFnEnum::Sync(f) = v {
                    f(wrapper_cloned)?;
                } else {
                    return Err(BeanError::HasAsync(k));
                }
            }
        }
        Ok(AppContext {
            inner: Arc::new(self.inner.into_inner().expect("unexpected lock")),
        })
    }

    pub async fn build_all(self) -> Result<AppContext, BeanError> {
        {
            let wrapper = self.inner.lock().expect("unexpected lock");
            if let Some((uninit_meta, _)) = wrapper
                .bean_map
                .iter()
                .find(|(meta, bean)| !bean.initialized())
            {
                return Err(BeanError::NotInitialized(uninit_meta.clone()));
            }
            for (k, v) in self.init_fn_map {
                let wrapper_cloned = wrapper
                    .bean_map
                    .get(&k)
                    .expect("unexpected meta key error")
                    .clone();
                match v {
                    InitFnEnum::Sync(f) => {
                        f(wrapper_cloned)?;
                    }
                    InitFnEnum::Async(fut) => {
                        fut(wrapper_cloned).await?;
                    }
                }
            }
        }
        Ok(AppContext {
            inner: Arc::new(self.inner.into_inner().expect("unexpected lock")),
        })
    }
}

/// the wrapper of initialization function
pub(crate) type InitFn = Box<dyn Fn(BeanWrapper) -> Result<(), BeanError>>;
pub(crate) type InitFnAsync =
    Box<dyn Fn(BeanWrapper) -> Pin<Box<dyn Future<Output = Result<(), BeanError>>>>>;

pub(crate) enum InitFnEnum {
    Sync(InitFn),
    Async(InitFnAsync),
}

/// helper method to build `InitFn`
pub(crate) fn build_init_fn<Props, E1, E2, T>() -> InitFnEnum
where
    E2: Send + Sync + 'static,
    T: BuildFromContext<Props, E1, E2> + Send + Sync + 'static,
{
    InitFnEnum::Sync(Box::new(|wrap| {
        let r = wrap.build_bean_ref::<T>().acquire();
        match r.init_self() {
            Ok(_) => Ok(()),
            Err(e) => Err(BeanError::DuringInit(wrap.meta.clone(), Box::new(e))),
        }
    }))
}

/// helper method to build `InitFnAsync`
pub(crate) fn build_init_fn_async<Props, E1, E2, T>() -> InitFnEnum
where
    E2: Send + Sync + 'static,
    T: BuildFromContextAsync<Props, E1, E2> + Send + Sync + 'static,
{
    InitFnEnum::Async(Box::new(|wrap| {
        Box::pin(async move {
            let r = wrap.build_bean_ref::<T>().acquire();
            match r.init_self().await {
                Ok(_) => Ok(()),
                Err(e) => Err(BeanError::DuringInit(wrap.meta.clone(), Box::new(e))),
            }
        })
    }))
}

/// the context to store all beans
/// `AppContext` owns the ownership of all registered beans
/// When `AppContext` is dropped, all beans will be dropped too
pub struct AppContext {
    inner: Arc<AppContextInner>,
}

impl Clone for AppContext {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl AppContext {
    /// the method `try_acquire_bean` can be used to get bean from runtime,
    /// like `ApplicationContext` in SpringBoot
    pub fn try_acquire_bean<T>(&self, name: &'static str) -> Option<BeanRef<T>>
    where
        T: Send + Sync + 'static,
    {
        let meta = BeanMetadata::build_meta::<T>(name);

        self.inner
            .bean_map
            .get(&meta)
            .cloned()
            .map(|w| w.build_bean_ref())
    }

    pub fn acquire_bean<T>(&self, _ty: BeanType<T>, name: &'static str) -> BeanRef<T>
    where
        T: Send + Sync + 'static,
    {
        self.try_acquire_bean(name)
            .expect("bean is not initialized")
    }

    /// the method `try_acquire_beans_by_type` can be used to get multiple beans with same type
    pub fn acquire_beans_by_type<T>(&self, _ty: BeanType<T>) -> Vec<BeanRef<T>>
    where
        T: Send + Sync + 'static,
    {
        self.inner
            .bean_map
            .iter()
            .filter(|(k, v)| k.type_name == type_name::<T>())
            .map(|(k, v)| v.clone().build_bean_ref())
            .collect()
    }

    /// the method `try_acquire_beans_by_name` can be used to get multiple beans with same name
    pub fn acquire_beans_by_name<T>(&self, name: &'static str) -> Vec<BeanRef<T>>
    where
        T: Send + Sync + 'static,
    {
        self.inner
            .bean_map
            .iter()
            .filter(|(k, v)| k.bean_name == name)
            .map(|(k, v)| v.clone().build_bean_ref())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use async_trait::async_trait;
    use std::time::Duration;

    pub struct ServiceA {
        svc_b: BeanRef<ServiceB>,

        dao: BeanRef<DaoC>,
    }

    impl ServiceA {
        pub fn check(&self) {
            println!("svc a is ready");
        }
    }

    impl Drop for ServiceA {
        fn drop(&mut self) {
            println!("svc a is dropped");
        }
    }

    impl BuildFromContext<(), ()> for ServiceA {
        fn build_from(ctx: &AppContextBuilder, extras: ()) -> Result<Self, ()> {
            Ok(ServiceA {
                svc_b: ctx.acquire_bean_or_init(ServiceB::BEAN_TYPE, "b"),

                dao: ctx.acquire_bean_or_init(DaoC::BEAN_TYPE, "c"),
            })
        }
    }

    pub struct ServiceB {
        svc_a: BeanRef<ServiceA>,

        dao: BeanRef<DaoC>,

        config_val: u32,
    }

    impl Drop for ServiceB {
        fn drop(&mut self) {
            println!("svc b is dropped");
        }
    }

    impl ServiceB {
        pub fn check(&self) {
            println!("svc b is ready");
        }
    }

    impl BuildFromContext<u32, (), anyhow::Error> for ServiceB {
        fn build_from(ctx: &AppContextBuilder, extras: u32) -> Result<Self, ()> {
            Ok(ServiceB {
                svc_a: ctx.acquire_bean_or_init(ServiceA::BEAN_TYPE, "a"),
                dao: ctx.acquire_bean_or_init(DaoC::BEAN_TYPE, "c"),
                config_val: extras,
            })
        }

        fn init_self(&self) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    pub struct DaoC {
        inner_map: HashMap<String, String>,
    }

    impl Drop for DaoC {
        fn drop(&mut self) {
            println!("dao c is dropped");
        }
    }

    impl DaoC {
        pub fn check(&self) {
            println!("dao c is ready");
        }
    }

    impl BuildFromContext<HashMap<String, String>, ()> for DaoC {
        fn build_from(
            ctx: &AppContextBuilder,
            extras: HashMap<String, String>,
        ) -> Result<Self, ()> {
            Ok(DaoC { inner_map: extras })
        }
    }

    pub struct DaoD {
        inner_vec: Vec<i32>,
    }

    impl Drop for DaoD {
        fn drop(&mut self) {
            println!("dao d is droped");
        }
    }

    impl DaoD {
        pub async fn check(&self) {
            println!("dao d is ready");
        }
    }

    #[async_trait]
    impl BuildFromContextAsync<usize, String> for DaoD {
        async fn build_from(ctx: &AppContextBuilder, extras: usize) -> Result<Self, String> {
            Ok(DaoD {
                inner_vec: Vec::with_capacity(extras),
            })
        }

        async fn init_self(&self) -> Result<(), ()> {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn it_works() -> anyhow::Result<()> {
        let svc_a = {
            //register beans with circular references
            let ctx = AppContextBuilder::new()
                .construct_bean(ServiceA::BEAN_TYPE, "a", ())
                .unwrap()
                .construct_bean(ServiceB::BEAN_TYPE, "b", 32)
                .unwrap()
                .construct_bean(DaoC::BEAN_TYPE, "c", HashMap::new())
                .unwrap()
                .construct_bean_async(DaoD::BEAN_TYPE, "d", 5_usize)
                .await
                .unwrap()
                .build_all()
                .await?;

            //test each bean
            let svc_a = ctx.acquire_bean(ServiceA::BEAN_TYPE, "a");
            svc_a.acquire().check();

            let svc_b = ctx.acquire_bean(ServiceB::BEAN_TYPE, "b");
            svc_b.acquire().check();

            let dao_c = ctx.acquire_bean(DaoC::BEAN_TYPE, "c");
            dao_c.acquire().check();

            let dao_d = ctx.acquire_bean(DaoD::BEAN_TYPE, "d");
            dao_d.acquire().check().await;

            //finally, all beans should be dropped

            svc_a
        };
        //if the app context is dropped, all beans become invalid
        assert!(!svc_a.is_active());

        //there will be an error if some beans are not set
        let ctx = AppContextBuilder::new()
            .construct_bean(ServiceA::BEAN_TYPE, "a", ())
            .unwrap()
            .build_without_init();
        assert!(ctx.is_err());

        Ok(())
    }
}
