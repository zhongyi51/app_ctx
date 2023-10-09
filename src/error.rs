use crate::BeanMetadata;
use std::any::Any;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EventError {
    /// if an event does not has any listener
    #[error("event with type `{0}` does not have any listener")]
    NoListener(&'static str),

    /// if the executor is not sync
    #[error("event with type `{0}` only has async listener, which cannot be run in sync context")]
    AsyncListener(&'static str),

    /// if the executor is not sync
    #[error("event with type `{0}` only has sync listener, which cannot be run in async context")]
    SyncListener(&'static str),
}

#[derive(Error, Debug)]
pub enum BeanError {
    /// error type for bean not initialized
    #[error("bean with type {} and name {} is not initialized with `construct_bean` method", .0.type_name, .0.bean_name)]
    NotInitialized(BeanMetadata),

    /// error type for the ignorance of async beans
    #[error("bean with type {} and name {} is async and cannot be initialized with `build_non_async` method", .0.type_name, .0.bean_name)]
    HasAsync(BeanMetadata),

    /// error type for cyclic dependencies
    #[error("cyclic dependencies detected: {}", pretty_print(.0))]
    CyclicDependency(Vec<BeanMetadata>),

    /// user custom initialization error
    #[error("custom error of bean {} with type {}: {}", .0.type_name, .0.bean_name, .1)]
    Custom(BeanMetadata, Box<dyn Error + Send + Sync>),
}

fn pretty_print(v: &Vec<BeanMetadata>) -> String {
    v.iter()
        .map(|m| format!("{}({})", m.bean_name, m.type_name))
        .collect::<Vec<_>>()
        .join(" -> ")
}

impl BeanError {
    /// get internal error object when custom error occurs
    pub fn into_internal_err<T>(self) -> Option<T>
    where
        T: Error + 'static,
    {
        if let BeanError::Custom(m, e) = self {
            return e.downcast::<T>().ok().map(|x| *x);
        }
        return None;
    }
}

/// error type for acquire bean after related `AppContext` is dropped
pub struct AppContextDroppedError;

impl Debug for AppContextDroppedError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "application context is dropped, all related beans are dropped too"
        )
    }
}

impl Display for AppContextDroppedError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "application context is dropped, all related beans are dropped too"
        )
    }
}

impl Error for AppContextDroppedError {}
