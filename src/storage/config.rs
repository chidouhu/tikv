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

const DEFAULT_STORE_PATH: &'static str = "";
const DEFAULT_SCHED_CAPACITY: usize = 4096;
const DEFAULT_SCHED_MSG_PER_TICK: usize = 256;
const DEFAULT_SCHED_CONCURRENCY: usize = 1024;

#[derive(Clone, Debug)]
pub struct Config {
    pub path: String,
    pub sched_notify_capacity: usize,
    pub sched_msg_per_tick: usize,
    pub sched_concurrency: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            path: DEFAULT_STORE_PATH.to_owned(),
            sched_notify_capacity: DEFAULT_SCHED_CAPACITY,
            sched_msg_per_tick: DEFAULT_SCHED_MSG_PER_TICK,
            sched_concurrency: DEFAULT_SCHED_CONCURRENCY,
        }
    }
}

impl Config {
    pub fn new() -> Config {
        Config::default()
    }
}