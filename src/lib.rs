mod error;
mod util;

use crate::error::{AppContextDroppedError, BeanError};
use once_cell::sync::OnceCell;
use std::any::{type_name, Any};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::fs::Metadata;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use crossbeam::atomic::AtomicCell;

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

/// the priority of beans.
/// the beans with higher priority will be initialized earlier
/// and destroyed later
pub enum BeanPriority{

    /// dependencies of this bean
    DependsOn(Vec<BeanMetadata>),

}

/// A trait for beans which can be created from `AppContextBuilder`.
/// `ctx` is the AppContext for acquiring lazy-initialized beans,
/// and `extras` are extra params for this build.
pub trait BuildFromContext<E, CtxErr = ()> {
    /// build the beans from
    fn build_from(ctx: &AppContextBuilder, extras: E) -> Result<Self, CtxErr>
    where
        Self: Sized;

    /// initialization method after all beans have been built.
    /// because of potential cyclic invocations during the initialization, only
    /// immutable references are allowed
    fn init_self(&self) -> Result<(), Box<dyn Error+Send+Sync>> {
        return Ok(());
    }

    /// finalize method before beans' destruction.
    /// this function will not be automatically called if you do not
    /// explicitly invoke `destroy_non_async`
    fn clean_self(&self) -> Result<(),Box<dyn Error+Send+Sync>>{
        return Ok(());
    }

    /// the priority of the this bean.
    /// default is empty
    fn priority() -> BeanPriority{
        return BeanPriority::DependsOn(Vec::new());
    }
}

/// async implementation for building context
#[async_trait::async_trait]
pub trait BuildFromContextAsync<E, CtxErr = ()> {
    async fn build_from(ctx: &AppContextBuilder, extras: E) -> Result<Self, CtxErr>
    where
        Self: Sized;


    /// initialization method after all beans have been built.
    /// because of potential cyclic invocations during the initialization, only
    /// immutable references are allowed
    async fn init_self(&self) -> Result<(), Box<dyn Error+Send+Sync>> {
        return Ok(());
    }

    /// finalize method before beans' destruction.
    /// this function will not be automatically called if you do not
    /// explicitly invoke `destroy_non_async`
    async fn clean_self(&self) -> Result<(),Box<dyn Error+Send+Sync>>{
        return Ok(());
    }

    /// the priority of the this bean.
    /// default is empty
    fn priority() -> BeanPriority{
        return BeanPriority::DependsOn(Vec::new());
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

/// status of a bean.
/// a bean without construction is `NotInit`,
/// after initialization, it becomes `PostConstruct`,
/// after running of `PostConstruct`, it becomes `Using`,
/// after running of `PreDestroy`, it becomes `Destroyed`
/// after dropping of the bean, it becomes `Inactive`
#[derive(Clone,Copy,Eq,PartialEq,Debug)]
pub enum BeanStatus{
    NotInit,
    PostConstruct,
    Using,
    Destroyed,
    Inactive
}

/// a wrapper for `Arc<dyn Any +Send +Sync>`, to store helper data
pub struct BeanWrapper {
    bean: Arc<dyn Any + Send + Sync>,
    status:AtomicCell<BeanStatus>,
    meta: BeanMetadata,
    is_async:OnceCell<bool>,
    init_fn:OnceCell<CustomFnEnum>,
    destroy_fn:OnceCell<CustomFnEnum>,
    depends_on:OnceCell<Vec<BeanMetadata>>,

}

impl Clone for BeanWrapper {
    fn clone(&self) -> Self {
        Self {
            bean: self.bean.clone(),
            status: AtomicCell::new(self.status.load()),
            meta: self.meta.clone(),
            is_async:self.is_async.clone(),
            init_fn: self.init_fn.clone(),
            destroy_fn: self.destroy_fn.clone(),
            depends_on: self.depends_on.clone(),
        }
    }
}

impl BeanWrapper {
    /// wrap a `OnceCell` and meta, to erase its type
    pub(crate) fn empty<T>(bean: OnceCell<T>, meta: BeanMetadata) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            status:AtomicCell::new(BeanStatus::NotInit),
            bean: Arc::new(bean),
            meta,
            is_async:OnceCell::new(),
            init_fn: OnceCell::new(),
            destroy_fn: OnceCell::new(),
            depends_on: OnceCell::new(),
        }
    }

    pub(crate) fn sync_one<T,E,Err>(bean: OnceCell<T>, meta: BeanMetadata) -> Self
        where
            T: BuildFromContext<E,Err> + Send + Sync + 'static,
    {
        Self {
            status:AtomicCell::new(BeanStatus::PostConstruct),
            bean: Arc::new(bean),
            meta,
            is_async:OnceCell::with_value(false),
            init_fn: OnceCell::with_value(build_init_fn::<E, Err, T>()),
            destroy_fn: OnceCell::with_value(build_destroy_fn::<E,Err,T>()),
            depends_on: OnceCell::with_value({
                if let BeanPriority::DependsOn(v)=T::priority(){
                    v
                }else {
                    vec![]
                }
            }),
        }
    }

    pub(crate) fn async_one<T,E,Err>(bean: OnceCell<T>, meta: BeanMetadata) -> Self
        where
            T: BuildFromContextAsync<E,Err> + Send + Sync + 'static,
    {
        Self {
            status:AtomicCell::new(BeanStatus::PostConstruct),
            bean: Arc::new(bean),
            meta,
            is_async:OnceCell::with_value(true),
            init_fn: OnceCell::with_value(build_init_fn_async::<E, Err, T>()),
            destroy_fn: OnceCell::with_value(build_destroy_fn_async::<E,Err,T>()),
            depends_on: OnceCell::with_value({
                if let BeanPriority::DependsOn(v)=T::priority(){
                    v
                }else {
                    vec![]
                }
            }),
        }
    }


    /// because we do not know its type, we cannot downcast
    /// it into OnceCell, so we need to use an extra field
    pub(crate) fn initialized(&self) -> bool {
        self.status.load() != BeanStatus::NotInit
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
    pub(crate) fn set_inner_sync<T,E,Err>(&self, bean: T)
    where
        T: BuildFromContext<E,Err>+Send + Sync + 'static,
    {
        self.bean
            .clone()
            .downcast::<OnceCell<T>>()
            .ok()
            .map(|c| c.set(bean).ok())
            .flatten()
            .expect("bean is set before");
        self.init_fn.set(build_init_fn::<E,Err,T>()).expect("bean is set before");
        self.destroy_fn.set(build_destroy_fn::<E,Err,T>()).expect("bean is set before");
        self.depends_on.set({
            if let BeanPriority::DependsOn(v)=T::priority(){
                v
            }else {
                vec![]
            }
        }).expect("bean is set before");
        self.is_async.set(false).expect("bean is set before");
        self.status.swap(BeanStatus::PostConstruct);
    }

    /// wrap the bean into it
    pub(crate) fn set_inner_async<T,E,Err>(&self, bean: T)
        where
            T: BuildFromContextAsync<E,Err>+Send + Sync + 'static,
    {
        self.bean
            .clone()
            .downcast::<OnceCell<T>>()
            .ok()
            .map(|c| c.set(bean).ok())
            .flatten()
            .expect("bean is set before");
        self.init_fn.set(build_init_fn_async::<E,Err,T>()).expect("bean is set before");
        self.destroy_fn.set(build_destroy_fn_async::<E,Err,T>()).expect("bean is set before");
        self.depends_on.set({
            if let BeanPriority::DependsOn(v)=T::priority(){
                v
            }else {
                vec![]
            }
        }).expect("bean is set before");
        self.is_async.set(true).expect("bean is set before");
        self.status.swap(BeanStatus::PostConstruct);
    }

    pub(crate) fn run_init_fn(&self)->Result<(),BeanError>{
        let f_enum=self.init_fn
            .get()
            .expect("unexpected get error");
        if let CustomFnEnum::Sync(f)=f_enum{
            f(self.clone())?;
        }else{
            panic!("invoke async function in sync context");
        }
        Ok(())
    }

    pub(crate) async fn run_init_fn_async(&self)->Result<(),BeanError>{
        let f_enum=self.init_fn
            .get()
            .expect("unexpected get error");
        if let CustomFnEnum::Async(f)=f_enum{
            f(self.clone()).await?;
        }else{
            panic!("invoke sync function in async context");
        }
        Ok(())
    }

    pub(crate) fn run_destroy_fn(&self)->Result<(),BeanError>{
        let f_enum=self.init_fn
            .get()
            .expect("unexpected get error");
        if let CustomFnEnum::Sync(f)=f_enum{
            f(self.clone())?;
        }else{
            panic!("invoke async function in sync context");
        }
        Ok(())
    }

    pub(crate) async fn run_destroy_fn_async(&self)->Result<(),BeanError>{
        let f_enum=self.init_fn
            .get()
            .expect("unexpected get error");
        if let CustomFnEnum::Async(f)=f_enum{
            f(self.clone()).await?;
        }else{
            panic!("invoke sync function in async context");
        }
        Ok(())
    }

    pub(crate) fn is_async(&self)->bool{
        *self.is_async
            .get()
            .expect("bean is not set")
    }
}

/// the inner storage for beans.
/// the `Arc<...>` stores beans with lazy initialization
pub struct AppContextInner {
    bean_map: HashMap<BeanMetadata, BeanWrapper>,
}

/// the context to store all beans.
/// `AppContext` owns the ownership of all registered beans.
/// When `AppContext` is dropped, all beans will be dropped too
pub struct AppContextBuilder {
    inner: Mutex<AppContextInner>,
}

impl AppContextBuilder {
    /// init method
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AppContextInner {
                bean_map: Default::default(),
            }),
        }
    }

    /// acquire the `BeanRef` of a bean.
    /// because of the initialization order, returned `BeanRef` may not be initialized.
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
            .or_insert(BeanWrapper::empty(OnceCell::<T>::new(), meta))
            .build_bean_ref()
    }

    /// construct a bean and hand over to `AppContextBuilder`.
    /// the bean type must implement `BuildFromContext`
    pub fn construct_bean<T, E, Err>(
        mut self,
        _ty: BeanType<T>,
        name: &'static str,
        extras: E,
    ) -> Result<Self, Err>
    where
        T: Send + Sync + BuildFromContext<E, Err> + 'static,
    {
        let meta = BeanMetadata::build_meta::<T>(name);
        let bean = T::build_from(&self, extras)?;
        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::sync_one(OnceCell::<T>::new(), meta.clone()))
            .set_inner_sync(bean);
        Ok(self)
    }

    /// construct a bean and hand over to `AppContextBuilder`.
    /// the bean type must implement `BuildFromContextAsync`
    pub async fn construct_bean_async<T, E, Err>(
        mut self,
        _ty: BeanType<T>,
        name: &'static str,
        extras: E,
    ) -> Result<Self, Err>
    where
        T: Send + Sync + BuildFromContextAsync<E, Err> + 'static,
    {
        let meta = BeanMetadata::build_meta::<T>(name);
        let bean = T::build_from(&self, extras).await?;
        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::async_one(OnceCell::<T>::new(), meta.clone()))
            .set_inner_async(bean);
        Ok(self)
    }

    /// finish construction and create `AppContext` without Mutex.
    /// this method will go over all beans and ensure all beans are initialized,
    /// but the initialization method will not be ran
    pub fn build_without_init(self) -> Result<AppContext, BeanError> {
        self._check_init_finished()?;
        Ok(AppContext {
            inner: Arc::new(self.inner.into_inner().expect("unexpected lock")),
        })
    }

    fn _build_dep_tree(&self,reverse:bool) -> HashMap<BeanMetadata,Vec<BeanMetadata>>{
        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .iter()
            .flat_map(|(m,w)|{
                w.depends_on
                    .get()
                    .expect("unexpected init error")
                    .iter()
                    .map(|c|if reverse {(c.clone(),m.clone())}else{(m.clone(),c.clone())})
            })
            .fold(HashMap::new(),|mut s,(k,v)|{
                s.entry(k)
                    .or_insert(Vec::new())
                    .push(v);
                s
            })
    }

    fn _check_init_finished(&self) -> Result<(), BeanError> {
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
        Ok(())
    }

    fn _check_contains_async(&self)->Result<(),BeanError>{
        if let Some(err)=self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .values()
            .find(|s|s.is_async())
            .map(|s|BeanError::HasAsync(s.meta.clone())){
            return Err(err)
        }
        Ok(())
    }

    fn _init_recursive(beans:&impl Deref<Target=HashMap<BeanMetadata,BeanWrapper>>,dep_map:&HashMap<BeanMetadata,Vec<BeanMetadata>>, history:Vec<BeanMetadata>)->Result<(),BeanError>{
        match history.last(){
            None=>panic!("history must contains at least one bean"),
            Some(v) if dep_map.get(v).is_none()=>{
                // no-op
            }
            Some(v) if dep_map.get(v).unwrap().is_empty()=>{
                beans.get(v)
                    .expect("unexpected bean error")
                    .run_init_fn()?;
            }
            Some(v) =>{
                let metas=dep_map.get(v).unwrap();
                for meta in metas{
                    let mut cloned =history.clone();
                    cloned.push(meta.clone());
                    Self::_init_recursive(beans,dep_map,cloned)?;
                }
            }
        }
        Ok(())
    }

    pub fn build_non_async(self) -> Result<AppContext, BeanError> {
        self._check_init_finished()?;
        self._check_contains_async()?;
        {
            let dep_tree=self._build_dep_tree(false);
            let beans=self.inner.lock().expect("unexpected lock");
                todo!("implement this")
        }
        Ok(AppContext {
            inner: Arc::new(self.inner.into_inner().expect("unexpected lock")),
        })
    }

    pub async fn build_all(self) -> Result<AppContext, BeanError> {
        self._check_init_finished()?;
        Ok(AppContext {
            inner: Arc::new(self.inner.into_inner().expect("unexpected lock")),
        })
    }
}

pub(crate) type CustomFn = Arc<dyn Fn(BeanWrapper) -> Result<(), BeanError>>;
pub(crate) type CustomFnAsync =
    Arc<dyn Fn(BeanWrapper) -> Pin<Box<dyn Future<Output = Result<(), BeanError>>>>>;

#[derive(Clone)]
pub(crate) enum CustomFnEnum {
    Sync(CustomFn),
    Async(CustomFnAsync),
}

impl Debug for CustomFnEnum{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self{
            CustomFnEnum::Sync(_) => {
                write!(f,"`sync fn`")
            }
            CustomFnEnum::Async(_) => {
                write!(f,"`async fn`")
            }
        }
    }
}

/// helper method to build `InitFn`
pub(crate) fn build_init_fn<Props, E1,  T>() -> CustomFnEnum
where
    T: BuildFromContext<Props, E1> + Send + Sync + 'static,
{
    CustomFnEnum::Sync(Arc::new(|wrap| {
        let r = wrap.build_bean_ref::<T>().acquire();
        match r.init_self() {
            Ok(_) => Ok(()),
            Err(e) => Err(BeanError::Custom(wrap.meta.clone(), e)),
        }
    }))
}

pub(crate) fn build_destroy_fn<Props, E1,  T>() -> CustomFnEnum
    where
        T: BuildFromContext<Props, E1> + Send + Sync + 'static,
{
    CustomFnEnum::Sync(Arc::new(|wrap| {
        let r = wrap.build_bean_ref::<T>().acquire();
        match r.clean_self() {
            Ok(_) => Ok(()),
            Err(e) => Err(BeanError::Custom(wrap.meta.clone(), e)),
        }
    }))
}

/// helper method to build `InitFnAsync`
pub(crate) fn build_init_fn_async<Props, E1, T>() -> CustomFnEnum
where
    T: BuildFromContextAsync<Props, E1> + Send + Sync + 'static,
{
    CustomFnEnum::Async(Arc::new(|wrap| {
        Box::pin(async move {
            let r = wrap.build_bean_ref::<T>().acquire();
            match r.init_self().await {
                Ok(_) => Ok(()),
                Err(e) => Err(BeanError::Custom(wrap.meta.clone(), e)),
            }
        })
    }))
}

pub(crate) fn build_destroy_fn_async<Props, E1, T>() -> CustomFnEnum
    where
        T: BuildFromContextAsync<Props, E1> + Send + Sync + 'static,
{
    CustomFnEnum::Async(Arc::new(|wrap| {
        Box::pin(async move {
            let r = wrap.build_bean_ref::<T>().acquire();
            match r.clean_self().await {
                Ok(_) => Ok(()),
                Err(e) => Err(BeanError::Custom(wrap.meta.clone(), e)),
            }
        })
    }))
}

/// the context to store all beans.
/// `AppContext` owns the ownership of all registered beans.
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

    /// whether current `AppContext` do not contains any arc clone
    pub fn is_last_clone(&self)->bool{
        Arc::strong_count(&self.inner)==1
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

    impl BuildFromContext<u32, ()> for ServiceB {
        fn build_from(ctx: &AppContextBuilder, extras: u32) -> Result<Self, ()> {
            Ok(ServiceB {
                svc_a: ctx.acquire_bean_or_init(ServiceA::BEAN_TYPE, "a"),
                dao: ctx.acquire_bean_or_init(DaoC::BEAN_TYPE, "c"),
                config_val: extras,
            })
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

        async fn init_self(&self) -> Result<(), Box<dyn Error+Sync+Send>> {
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

            assert!(ctx.is_last_clone());
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
