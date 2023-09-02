use std::any::{type_name, Any};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Arc, Mutex,Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use once_cell::sync::OnceCell;

/// metadata of a bean. it can be used as map key
#[derive(Hash,Eq, PartialEq,Clone)]
pub struct BeanMetadata{
    /// typename of the bean, such as `XXXService`
    type_name:&'static str,
    /// an identifier for the bean, to distinguish different
    /// beans with same type name
    bean_name:&'static str
}

impl BeanMetadata{
    
    pub(crate) fn from_type<T>(name:&'static str)->BeanMetadata{
        BeanMetadata{
            type_name: type_name::<T>(),
            bean_name: name,
        }
    }
}

/// A trait for beans which can be created from `AppContextBuilder`.
/// `ctx` is the AppContext for acquiring lazy-initialized beans,
/// and `extras` are extra params for this build.
pub trait BuildFromContext<E,Err> {
    /// build the beans from
    fn build_from(ctx: &AppContextBuilder, extras: E) -> Result<Self,Err> where Self:Sized;
}


/// async implementation for building context
#[async_trait::async_trait]
pub trait BuildFromContextAsync<E,Err> {
    async fn build_from(ctx: &AppContextBuilder, extras: E) -> Result<Self,Err> where Self:Sized;
}

/// like `Class<T>` in java
pub struct BeanType<T>(PhantomData<T>);

pub trait BeanTypeOf<T>{

    const BEAN_TYPE:BeanType<T>;

}

impl<T> BeanTypeOf<T> for T{
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
    pub fn try_acquire(&self) -> Option<RefWrapper<T>> {
        let arc_ref = self
            .inner
            .upgrade()
            .map(|c|RefWrapper(c));
        arc_ref
    }

    /// acquire the bean, if corresponding app context is dropped,
    /// a panic will be thrown
    pub fn acquire(&self)->RefWrapper<T>{
        self.try_acquire()
            .expect("app context is dropped, all beans are not acquirable")
    }
}

/// a wrapper for `Arc<dyn Any +Send +Sync>`, to store helper data
pub struct BeanWrapper{
    bean:Arc<dyn Any+Send+Sync>,
    initialized:AtomicBool,
    meta:BeanMetadata
}

impl Clone for BeanWrapper{
    fn clone(&self) -> Self {
        Self{
            bean: self.bean.clone(),
            initialized: AtomicBool::new(self.initialized.load(Ordering::Acquire)),
            meta: self.meta.clone(),
        }
    }
}

impl BeanWrapper{

    /// wrap a `OnceCell` and meta, to erase its type
    pub(crate) fn wrap<T>(bean:OnceCell<T>,meta:BeanMetadata)->Self
    where T:Send+Sync+'static{
        Self{
            initialized: AtomicBool::new(bean.get().is_some()),
            bean: Arc::new(bean),
            meta,
        }
    }

    /// because we do not know its type, we cannot downcast
    /// it into OnceCell, so we need to use an extra field
    pub(crate) fn initialized(&self)->bool{
        self.initialized.load(Ordering::Acquire)
    }

    /// build `BeanRef<T>` from inner arc
    pub(crate) fn build_bean_ref<T>(&self)->BeanRef<T>
    where T:Send+Sync+'static{
        let weak_arc=self.bean
            .clone()
            .downcast::<OnceCell<T>>()
            .ok()
            .map(|c|Arc::downgrade(&c))
            .expect("bean type is not matched");
        BeanRef{
            inner: weak_arc,
        }
    }

    /// wrap the bean into it
    pub(crate) fn set_inner<T>(&self,bean:T)
    where T:Send+Sync+'static{
        self.bean
            .clone()
            .downcast::<OnceCell<T>>()
            .ok()
            .map(|c|c.set(bean).ok())
            .flatten()
            .expect("bean is setted before");
        self.initialized.store(true,Ordering::Release);
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

    /// the method `acquire_bean_or_init` only requires immutable reference, so
    /// the beans which implements `BuildFromContext` can invoke it during the construction
    pub fn acquire_bean_or_init<T>(&self, name: &'static str) -> BeanRef<T>
        where
            T: Send + Sync + 'static,
    {
        let meta=BeanMetadata::from_type::<T>(name);
        
        self
            .inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::wrap(OnceCell::<T>::new(),meta))
            .build_bean_ref()
    }

    /// in contrast, this method is only invoked during the initialization, to
    /// register all beans
    pub fn construct_bean<T, E,Err>(self, _ty: BeanType<T>, name: &'static str, extras: E) ->Result<Self,Err>
        where
            T: Send + Sync + BuildFromContext<E,Err> + 'static,
    {
        let meta=BeanMetadata::from_type::<T>(name);
        let bean = T::build_from(&self, extras)?;
        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::wrap(OnceCell::<T>::new(),meta.clone()))
            .set_inner(bean);
        Ok(self)
    }

    pub async fn construct_bean_async<T, E,Err>(self, _ty: BeanType<T>, name: &'static str, extras: E) ->Result<Self,Err>
        where
            T: Send + Sync + BuildFromContextAsync<E,Err> + 'static,
    {
        let meta=BeanMetadata::from_type::<T>(name);
        let bean = T::build_from(&self, extras).await?;
        self.inner
            .lock()
            .expect("unexpected lock")
            .bean_map
            .entry(meta.clone())
            .or_insert(BeanWrapper::wrap(OnceCell::<T>::new(),meta))
            .set_inner(bean);
        Ok(self)
    }

    /// when finish construction, we can create `AppContext` without Mutex
    /// this method will go over all beans and ensure all beans are initialized
    pub fn build(self)->AppContext{

        AppContext{
            inner: Arc::new(self.inner.into_inner().expect("unexpected lock error")),
        }
    }
}


/// the context to store all beans
/// `AppContext` owns the ownership of all registered beans
/// When `AppContext` is dropped, all beans will be dropped too
pub struct AppContext {
    inner: Arc<AppContextInner>,
}

impl Clone for AppContext{
    fn clone(&self) -> Self {
        Self{
            inner:self.inner.clone()
        }
    }
}

impl AppContext{


    /// the method `try_acquire_bean` can be used to get bean from runtime,
    /// like `ApplicationContext` in SpringBoot
    pub fn try_acquire_bean<T>(&self, name: &'static str) -> Option<BeanRef<T>>
        where
            T: Send + Sync + 'static,
    {
        let meta=BeanMetadata::from_type::<T>(name);

       self
            .inner
            .bean_map
            .get(&meta)
            .cloned()
            .map(|w|w.build_bean_ref())
    }

    pub fn acquire_bean<T>(&self, _ty: BeanType<T>, name: &'static str) -> BeanRef<T>
        where
            T: Send + Sync + 'static,{
        self.try_acquire_bean(name).expect("bean is not initialized")
    }

}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use super::*;

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

    impl BuildFromContext<(),()> for ServiceA {
        fn build_from(ctx: &AppContextBuilder, extras: ()) -> Result<Self,()> {
            Ok(ServiceA {
                svc_b: ctx.acquire_bean_or_init("b"),

                dao: ctx.acquire_bean_or_init("c"),
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

    impl BuildFromContext<u32,()> for ServiceB {
        fn build_from(ctx: &AppContextBuilder, extras: u32) -> Result<Self,()> {
            Ok(ServiceB {
                svc_a: ctx.acquire_bean_or_init("a"),
                dao: ctx.acquire_bean_or_init("c"),
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

    impl BuildFromContext<HashMap<String, String>,()> for DaoC {
        fn build_from(ctx: &AppContextBuilder, extras: HashMap<String, String>) -> Result<Self,()> {
            Ok(DaoC { inner_map: extras })
        }
    }

    pub struct DaoD{
        inner_vec:Vec<i32>
    }

    impl Drop for DaoD{
        fn drop(&mut self) {
            println!("dao d is droped");
        }
    }

    impl DaoD{
        pub async fn check(&self){
            println!("dao d is ready");
        }
    }

    #[async_trait]
    impl BuildFromContextAsync<usize,String> for DaoD{
        async fn build_from(ctx: &AppContextBuilder, extras: usize) -> Result<Self,String> {
            Ok(DaoD{
                inner_vec:Vec::with_capacity(extras)
            })
        }
    }

    #[tokio::test]
    async fn it_works() {
        let mut ctxb = AppContextBuilder::new();

        //register beans with circular references
        let ctx=AppContextBuilder::new()
            .construct_bean(ServiceA::BEAN_TYPE, "a", ())
            .unwrap()
            .construct_bean(ServiceB::BEAN_TYPE, "b", 32)
            .unwrap()
            .construct_bean(DaoC::BEAN_TYPE, "c", HashMap::new())
            .unwrap()
            .construct_bean_async(DaoD::BEAN_TYPE, "d", 5_usize)
            .await
            .unwrap()
            .build();

        //test each bean
        let svc_a = ctx.acquire_bean(ServiceA::BEAN_TYPE, "a");
        svc_a.acquire().check();

        let svc_b = ctx.acquire_bean(ServiceB::BEAN_TYPE, "b");
        svc_b.acquire().check();

        let dao_c = ctx.acquire_bean(DaoC::BEAN_TYPE, "c");
        dao_c.acquire().check();

        //finally, all beans should be dropped
    }
}
