// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::thread;
use std::boxed::FnBox;
use std::fmt;
use std::error;
use std::sync::Arc;
use std::io::Error as IoError;

use mio::{EventLoop, EventLoopBuilder};

pub mod engine;
pub mod mvcc;
pub mod txn;
pub mod config;
mod types;

pub use self::config::Config;
pub use self::engine::{Engine, Snapshot, Dsn, TEMP_DIR, new_engine, Modify, Cursor,
                       Error as EngineError};
pub use self::engine::raftkv::RaftKv;
pub use self::txn::{SnapshotStore, Scheduler, Msg, SchedCh};
pub use self::types::{Key, Value, KvPair};
pub type Callback<T> = Box<FnBox(Result<T>) + Send>;

pub type CfName = &'static str;
pub const DEFAULT_CFS: &'static [CfName] = &["default", "lock"];

#[cfg(test)]
pub use self::types::make_key;

#[derive(Debug, Clone)]
pub enum Mutation {
    Put((Key, Value)),
    Delete(Key),
    Lock(Key),
}

#[allow(match_same_arms)]
impl Mutation {
    pub fn key(&self) -> &Key {
        match *self {
            Mutation::Put((ref key, _)) => key,
            Mutation::Delete(ref key) => key,
            Mutation::Lock(ref key) => key,
        }
    }
}

use kvproto::kvrpcpb::Context;

#[allow(type_complexity)]
pub enum Command {
    Get {
        ctx: Context,
        key: Key,
        start_ts: u64,
        callback: Option<Callback<Option<Value>>>,
    },
    BatchGet {
        ctx: Context,
        keys: Vec<Key>,
        start_ts: u64,
        callback: Option<Callback<Vec<Result<KvPair>>>>,
    },
    Scan {
        ctx: Context,
        start_key: Key,
        limit: usize,
        start_ts: u64,
        callback: Option<Callback<Vec<Result<KvPair>>>>,
    },
    Prewrite {
        ctx: Context,
        mutations: Vec<Mutation>,
        primary: Vec<u8>,
        start_ts: u64,
        callback: Option<Callback<Vec<Result<()>>>>,
    },
    Commit {
        ctx: Context,
        keys: Vec<Key>,
        lock_ts: u64,
        commit_ts: u64,
        callback: Option<Callback<()>>,
    },
    CommitThenGet {
        ctx: Context,
        key: Key,
        lock_ts: u64,
        commit_ts: u64,
        get_ts: u64,
        callback: Option<Callback<Option<Value>>>,
    },
    Cleanup {
        ctx: Context,
        key: Key,
        start_ts: u64,
        callback: Option<Callback<()>>,
    },
    Rollback {
        ctx: Context,
        keys: Vec<Key>,
        start_ts: u64,
        callback: Option<Callback<()>>,
    },
    RollbackThenGet {
        ctx: Context,
        key: Key,
        lock_ts: u64,
        callback: Option<Callback<Option<Value>>>,
    },
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Command::Get { ref key, start_ts, .. } => {
                write!(f, "kv::command::get {} @ {}", key, start_ts)
            }
            Command::BatchGet { ref keys, start_ts, .. } => {
                write!(f, "kv::command_batch_get {} @ {}", keys.len(), start_ts)
            }
            Command::Scan { ref start_key, limit, start_ts, .. } => {
                write!(f,
                       "kv::command::scan {}({}) @ {}",
                       start_key,
                       limit,
                       start_ts)
            }
            Command::Prewrite { ref mutations, start_ts, .. } => {
                write!(f,
                       "kv::command::prewrite mutations({}) @ {}",
                       mutations.len(),
                       start_ts)
            }
            Command::Commit { ref keys, lock_ts, commit_ts, .. } => {
                write!(f,
                       "kv::command::commit {} {} -> {}",
                       keys.len(),
                       lock_ts,
                       commit_ts)
            }
            Command::CommitThenGet { ref key, lock_ts, commit_ts, get_ts, .. } => {
                write!(f,
                       "kv::command::commit_then_get {:?} {} -> {} @ {}",
                       key,
                       lock_ts,
                       commit_ts,
                       get_ts)
            }
            Command::Cleanup { ref key, start_ts, .. } => {
                write!(f, "kv::command::cleanup {} @ {}", key, start_ts)
            }
            Command::Rollback { ref keys, start_ts, .. } => {
                write!(f,
                       "kv::command::rollback keys({}) @ {}",
                       keys.len(),
                       start_ts)
            }
            Command::RollbackThenGet { ref key, lock_ts, .. } => {
                write!(f, "kv::rollback_then_get {} @ {}", key, lock_ts)
            }
        }
    }
}

impl Command {
    pub fn readonly(&self) -> bool {
        match *self {
            Command::Get { .. } |
            Command::BatchGet { .. } |
            Command::Scan { .. } => true,
            _ => false,
        }
    }
}

pub struct Storage {
    engine: Arc<Box<Engine>>,
    schedch: SchedCh,
    sched_handle: Option<thread::JoinHandle<()>>,
    sched_event_loop: Option<EventLoop<Scheduler>>,
}

impl Storage {
    pub fn from_engine(engine: Box<Engine>, config: &Config) -> Result<Storage> {
        let engine = Arc::new(engine);
        let event_loop = try!(create_event_loop(config.sched_notify_capacity,
                                                config.sched_msg_per_tick));
        let schedch = SchedCh::new(event_loop.channel());

        info!("storage {:?} started.", engine);
        Ok(Storage {
            engine: engine,
            schedch: schedch,
            sched_handle: None,
            sched_event_loop: Some(event_loop),
        })
    }

    pub fn new(config: &Config) -> Result<Storage> {
        let engine = try!(engine::new_engine(Dsn::RocksDBPath(&config.path), DEFAULT_CFS));
        Storage::from_engine(engine, config)
    }

    pub fn start(&mut self, config: &Config) -> Result<()> {
        if self.sched_handle.is_some() {
            return Err(box_err!("scheduler is already running"));
        }

        let engine = self.engine.clone();
        let builder = thread::Builder::new().name(thd_name!("storage-scheduler"));
        let mut sched_event_loop = self.sched_event_loop.take().unwrap();
        let sched_concurrency = config.sched_concurrency;
        let h = try!(builder.spawn(move || {
            let ch = SchedCh::new(sched_event_loop.channel());
            let mut sched = Scheduler::new(engine, ch, sched_concurrency);
            if let Err(e) = sched_event_loop.run(&mut sched) {
                panic!("scheduler run err:{:?}", e);
            }
            info!("scheduler stopped");
        }));
        self.sched_handle = Some(h);

        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.sched_handle.is_none() {
            return Ok(());
        }

        if let Err(e) = self.schedch.send(Msg::Quit) {
            error!("send quit cmd to scheduler failed, error:{:?}", e);
            return Err(Error::from(e));
        }

        let h = self.sched_handle.take().unwrap();
        if let Err(e) = h.join() {
            return Err(box_err!("failed to join sched_handle, err:{:?}", e));
        }

        info!("storage {:?} closed.", self.engine);
        Ok(())
    }

    pub fn get_engine(&self) -> Arc<Box<Engine>> {
        self.engine.clone()
    }

    fn send(&self, cmd: Command) -> Result<()> {
        if self.sched_handle.is_some() {
            try!(self.schedch.send(Msg::RawCmd { cmd: cmd }));
        } else {
            return Err(Error::Closed);
        }
        Ok(())
    }

    pub fn async_get(&self,
                     ctx: Context,
                     key: Key,
                     start_ts: u64,
                     callback: Callback<Option<Value>>)
                     -> Result<()> {
        let cmd = Command::Get {
            ctx: ctx,
            key: key,
            start_ts: start_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_batch_get(&self,
                           ctx: Context,
                           keys: Vec<Key>,
                           start_ts: u64,
                           callback: Callback<Vec<Result<KvPair>>>)
                           -> Result<()> {
        let cmd = Command::BatchGet {
            ctx: ctx,
            keys: keys,
            start_ts: start_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_scan(&self,
                      ctx: Context,
                      start_key: Key,
                      limit: usize,
                      start_ts: u64,
                      callback: Callback<Vec<Result<KvPair>>>)
                      -> Result<()> {
        let cmd = Command::Scan {
            ctx: ctx,
            start_key: start_key,
            limit: limit,
            start_ts: start_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_prewrite(&self,
                          ctx: Context,
                          mutations: Vec<Mutation>,
                          primary: Vec<u8>,
                          start_ts: u64,
                          callback: Callback<Vec<Result<()>>>)
                          -> Result<()> {
        let cmd = Command::Prewrite {
            ctx: ctx,
            mutations: mutations,
            primary: primary,
            start_ts: start_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_commit(&self,
                        ctx: Context,
                        keys: Vec<Key>,
                        lock_ts: u64,
                        commit_ts: u64,
                        callback: Callback<()>)
                        -> Result<()> {
        let cmd = Command::Commit {
            ctx: ctx,
            keys: keys,
            lock_ts: lock_ts,
            commit_ts: commit_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_commit_then_get(&self,
                                 ctx: Context,
                                 key: Key,
                                 lock_ts: u64,
                                 commit_ts: u64,
                                 get_ts: u64,
                                 callback: Callback<Option<Value>>)
                                 -> Result<()> {
        let cmd = Command::CommitThenGet {
            ctx: ctx,
            key: key,
            lock_ts: lock_ts,
            commit_ts: commit_ts,
            get_ts: get_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_cleanup(&self,
                         ctx: Context,
                         key: Key,
                         start_ts: u64,
                         callback: Callback<()>)
                         -> Result<()> {
        let cmd = Command::Cleanup {
            ctx: ctx,
            key: key,
            start_ts: start_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_rollback(&self,
                          ctx: Context,
                          keys: Vec<Key>,
                          start_ts: u64,
                          callback: Callback<()>)
                          -> Result<()> {
        let cmd = Command::Rollback {
            ctx: ctx,
            keys: keys,
            start_ts: start_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }

    pub fn async_rollback_then_get(&self,
                                   ctx: Context,
                                   key: Key,
                                   lock_ts: u64,
                                   callback: Callback<Option<Value>>)
                                   -> Result<()> {
        let cmd = Command::RollbackThenGet {
            ctx: ctx,
            key: key,
            lock_ts: lock_ts,
            callback: Some(callback),
        };
        try!(self.send(cmd));
        Ok(())
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Engine(err: EngineError) {
            from()
            cause(err)
            description(err.description())
        }
        Txn(err: txn::Error) {
            from()
            cause(err)
            description(err.description())
        }
        Closed {
            description("storage is closed.")
        }
        Other(err: Box<error::Error + Send + Sync>) {
            from()
            cause(err.as_ref())
            description(err.description())
        }
        Io(err: IoError) {
            from()
            cause(err)
            description(err.description())
        }
    }
}

pub type Result<T> = ::std::result::Result<T, Error>;

pub fn create_event_loop(notify_capacity: usize,
                         messages_per_tick: usize)
                         -> Result<EventLoop<Scheduler>> {
    let mut builder = EventLoopBuilder::new();
    builder.notify_capacity(notify_capacity);
    builder.messages_per_tick(messages_per_tick);
    let el = try!(builder.build());
    Ok(el)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Sender};
    use kvproto::kvrpcpb::Context;

    fn expect_get_none(done: Sender<i32>) -> Callback<Option<Value>> {
        Box::new(move |x: Result<Option<Value>>| {
            assert_eq!(x.unwrap(), None);
            done.send(1).unwrap();
        })
    }

    fn expect_get_val(done: Sender<i32>, v: Vec<u8>) -> Callback<Option<Value>> {
        Box::new(move |x: Result<Option<Value>>| {
            assert_eq!(x.unwrap().unwrap(), v);
            done.send(1).unwrap();
        })
    }

    fn expect_ok<T>(done: Sender<i32>) -> Callback<T> {
        Box::new(move |x: Result<T>| {
            assert!(x.is_ok());
            done.send(1).unwrap();
        })
    }

    fn expect_fail<T>(done: Sender<i32>) -> Callback<T> {
        Box::new(move |x: Result<T>| {
            assert!(x.is_err());
            done.send(1).unwrap();
        })
    }

    fn expect_scan(done: Sender<i32>, pairs: Vec<Option<KvPair>>) -> Callback<Vec<Result<KvPair>>> {
        Box::new(move |rlt: Result<Vec<Result<KvPair>>>| {
            let rlt: Vec<Option<KvPair>> = rlt.unwrap()
                .into_iter()
                .map(Result::ok)
                .collect();
            assert_eq!(rlt, pairs);
            done.send(1).unwrap()
        })
    }

    #[test]
    fn test_get_put() {
        let config = Config::new();
        let mut storage = Storage::new(&config).unwrap();
        storage.start(&config).unwrap();
        let (tx, rx) = channel();
        storage.async_get(Context::new(),
                       make_key(b"x"),
                       100,
                       expect_get_none(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        storage.async_prewrite(Context::new(),
                            vec![Mutation::Put((make_key(b"x"), b"100".to_vec()))],
                            b"x".to_vec(),
                            100,
                            expect_ok(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        storage.async_commit(Context::new(),
                          vec![make_key(b"x")],
                          100,
                          101,
                          expect_ok(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        storage.async_get(Context::new(),
                       make_key(b"x"),
                       100,
                       expect_get_none(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        storage.async_get(Context::new(),
                       make_key(b"x"),
                       101,
                       expect_get_val(tx.clone(), b"100".to_vec()))
            .unwrap();
        rx.recv().unwrap();
        storage.stop().unwrap();
    }
    #[test]
    fn test_scan() {
        let config = Config::new();
        let mut storage = Storage::new(&config).unwrap();
        storage.start(&config).unwrap();
        let (tx, rx) = channel();
        storage.async_prewrite(Context::new(),
                            vec![
            Mutation::Put((make_key(b"a"), b"aa".to_vec())),
            Mutation::Put((make_key(b"b"), b"bb".to_vec())),
            Mutation::Put((make_key(b"c"), b"cc".to_vec())),
            ],
                            b"a".to_vec(),
                            1,
                            expect_ok(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        storage.async_commit(Context::new(),
                          vec![make_key(b"a"),make_key(b"b"),make_key(b"c"),],
                          1,
                          2,
                          expect_ok(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        storage.async_scan(Context::new(),
                        make_key(b"\x00"),
                        1000,
                        5,
                        expect_scan(tx.clone(),
                                    vec![
            Some((b"a".to_vec(), b"aa".to_vec())),
            Some((b"b".to_vec(), b"bb".to_vec())),
            Some((b"c".to_vec(), b"cc".to_vec())),
            ]))
            .unwrap();
        rx.recv().unwrap();
        storage.stop().unwrap();
    }

    #[test]
    fn test_txn() {
        let config = Config::new();
        let mut storage = Storage::new(&config).unwrap();
        storage.start(&config).unwrap();
        let (tx, rx) = channel();
        storage.async_prewrite(Context::new(),
                            vec![Mutation::Put((make_key(b"x"), b"100".to_vec()))],
                            b"x".to_vec(),
                            100,
                            expect_ok(tx.clone()))
            .unwrap();
        storage.async_prewrite(Context::new(),
                            vec![Mutation::Put((make_key(b"y"), b"101".to_vec()))],
                            b"y".to_vec(),
                            101,
                            expect_ok(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        rx.recv().unwrap();
        storage.async_commit(Context::new(),
                          vec![make_key(b"x")],
                          100,
                          110,
                          expect_ok(tx.clone()))
            .unwrap();
        storage.async_commit(Context::new(),
                          vec![make_key(b"y")],
                          101,
                          111,
                          expect_ok(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        rx.recv().unwrap();
        storage.async_get(Context::new(),
                       make_key(b"x"),
                       120,
                       expect_get_val(tx.clone(), b"100".to_vec()))
            .unwrap();
        storage.async_get(Context::new(),
                       make_key(b"y"),
                       120,
                       expect_get_val(tx.clone(), b"101".to_vec()))
            .unwrap();
        rx.recv().unwrap();
        rx.recv().unwrap();
        storage.async_prewrite(Context::new(),
                            vec![Mutation::Put((make_key(b"x"), b"105".to_vec()))],
                            b"x".to_vec(),
                            105,
                            expect_fail(tx.clone()))
            .unwrap();
        rx.recv().unwrap();
        storage.stop().unwrap();
    }
}
