//! The supported executors.
//!
//! An executor is something that implements the communication protocol for evaluating DAGs.
//! For example the `LocalExecutor` is an implementation of a thread-based executor that will listen
//! on the client channel and will spawn a list of local workers.
//!
//! # Example
//!
//! ```
//! use task_maker_store::FileStore;
//! use task_maker_exec::executors::LocalExecutor;
//! use std::sync::{Arc, Mutex, mpsc::channel};
//! # use std::thread;
//! # use tempdir::TempDir;
//! use task_maker_cache::Cache;
//!
//! # let tmpdir = TempDir::new("tm-test").unwrap();
//! # let path = tmpdir.path();
//! let store = FileStore::new(path).unwrap();
//! let cache = Cache::new(path).unwrap();
//! let num_cores = 4;
//! let mut executor = LocalExecutor::new(Arc::new(store), num_cores, path);
//! // the communication channels for the client
//! let (tx, rx_remote) = channel();
//! let (tx_remote, rx) = channel();
//!
//! # let server = thread::spawn(move || {
//! executor.evaluate(tx_remote, rx_remote, cache).unwrap();  // this will block!!
//! # });
//! # drop(tx);
//! # drop(rx);
//! # server.join().unwrap();
//! ```

mod local_executor;

pub use local_executor::*;
