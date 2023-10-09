use std::any::{Any, type_name};
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use crate::{AppContextInner};

/// event source.
/// an event can be omitted by `AppContext` or by user
#[derive(Clone)]
pub enum EventSource{
    AppCtx(Arc<AppContextInner>),
    Custom
}

/// app event struct
#[derive(Clone)]
pub struct AppEvent<T:Clone> {
    source:EventSource,
    timestamp:u128,
    is_async:bool,
    data:T
}

/// erased event listener for runtime
pub struct ErasedListener {
    is_async:bool,
    event_type:&'static str,
    inner:Arc<dyn Any+Send+Sync>
}

impl Clone for ErasedListener{
    fn clone(&self) -> Self {
        Self{
            is_async:self.is_async,
            event_type:self.event_type,
            inner:self.inner.clone()
        }
    }
}

impl ErasedListener{

    pub(crate) fn is_async(&self)->bool{
        self.is_async
    }

    pub(crate) fn sync_one<E:'static>(listener:impl AppEventListener<EventType=E>+Sync+Send+'static)->Self{
        let inner=Arc::new(Box::new(listener) as Box<dyn AppEventListener<EventType=E>+Sync+Send+'static>);
        return Self{
            is_async:false,
            event_type:type_name::<E>(),
            inner
        }
    }

    pub(crate) fn async_one<E:'static>(listener:impl AppEventListenerAsync<EventType=E>+Sync+Send+'static)->Self{
        let inner=Arc::new(Box::new(listener) as Box<dyn AppEventListenerAsync<EventType=E>+Sync+Send+'static>);
        return Self{
            is_async:true,
            event_type:type_name::<E>(),
            inner
        }
    }

    pub(crate) fn deal_sync<T:'static+Send>(&self,event:T,executor:&impl Deref<Target=dyn EventExecutor+Sync>){
        let r=self.inner.clone()
            .downcast::<Box<dyn AppEventListener<EventType=T>+Send+Sync+'static>>()
            .expect("unexpected cast error");
        executor.execute(Box::new(move ||r.on_event(event)));
    }

    pub(crate) async fn deal_async<T:'static+Send>(&self,event:T,executor:&impl Deref<Target=dyn EventExecutorAsync+Sync>){
        let r=self.inner.clone()
            .downcast::<Box<dyn AppEventListenerAsync<EventType=T>+Send+Sync+'static>>()
            .expect("unexpected cast error");
        executor.execute(Box::pin(async move{r.on_event_async(event).await;})).await;
    }

}

/// a listener for dealing sync events
pub trait AppEventListener{
    type EventType;

    fn on_event(&self, event:Self::EventType);

}

/// basic wrapper for a single function
pub struct FnListener<E,F>
    where F:Fn(E){
    _event:PhantomData<E>,
    f:F
}

impl<E,F> FnListener<E,F>
    where F:Fn(E){
    pub fn wrap(f:F)->Self{
        Self{
            _event:PhantomData::default(),
            f
        }
    }
}

impl<E,F> AppEventListener for FnListener<E,F>
    where F:Fn(E){
    type EventType = E;

    fn on_event(&self, event: Self::EventType) {
        (self.f)(event);
    }
}

/// async version of `AppEventListener`
#[async_trait::async_trait]
pub trait AppEventListenerAsync{
    type EventType;

    async fn on_event_async(&self, event:Self::EventType);
}


/// basic wrapper for a single function
pub struct FnAsyncListener<E,F,Fut>
    where F:Fn(E)->Fut,
Fut:Future<Output=()>{
    _event:PhantomData<E>,
    _fut:PhantomData<Fut>,
    f:F
}

impl<E,F,Fut> FnAsyncListener<E,F,Fut>
    where F:Fn(E)->Fut,
          Fut:Future<Output=()>{
    pub fn wrap(f:F)->Self{
        Self{
            _event:PhantomData::default(),
            _fut: PhantomData::default(),
            f
        }
    }
}

#[async_trait::async_trait]
impl<E,F,Fut> AppEventListenerAsync for FnAsyncListener<E,F,Fut>
    where F:Fn(E)->Fut+Sync+Send,
          Fut:Future<Output=()>+Sync+Send,
E:Send+Sync{
    type EventType = E;

    async fn on_event_async(&self, event: Self::EventType) {
        (self.f)(event).await;
    }
}

/// an executor to execute the events.
/// it can be either sync or async
pub trait EventExecutor{

    fn execute(&self,f:Box<dyn FnOnce()+Send>);

}

/// async version of `EventExecutor`
#[async_trait::async_trait]
pub trait EventExecutorAsync{

    async fn execute(&self,f:Pin<Box<dyn Future<Output=()>+Send>>);

}

/// default implementation of executor.
/// because different async runtimes have different executors,
/// default executor only deal events synchronously
#[derive(Clone)]
pub struct DefaultExecutor;

impl EventExecutor for DefaultExecutor{
    fn execute(&self, f: Box<dyn FnOnce()+Send>) {
        f();
    }
}

#[async_trait::async_trait]
impl EventExecutorAsync for DefaultExecutor{
    async fn execute(&self, f: Pin<Box<dyn Future<Output=()>+Send>>) {
        f.await;
    }
}
