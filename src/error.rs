use crate::BeanMetadata;
use std::any::Any;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BeanError {
    /// error type for bean not initialized
    #[error("bean with type {} and name {} is not initialized with `construct_bean` method", .0.type_name, .0.bean_name)]
    NotInitialized(BeanMetadata),

    /// error type for the ignorance of async beans
    #[error("bean with type {} and name {} is async and cannot be initialized with `build_non_async` method", .0.type_name, .0.bean_name)]
    HasAsync(BeanMetadata),

    /// user custom initialization error
    #[error("an error occurred during initialization of bean {} with type {}", .0.type_name, .0.bean_name)]
    DuringInit(BeanMetadata, Box<dyn Any + Send + Sync>),
}

impl BeanError {
    /// get internal error object when error occurs during initialization
    pub fn into_internal_err<T>(self) -> Option<T>
    where
        T: 'static,
    {
        if let BeanError::DuringInit(m, e) = self {
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
