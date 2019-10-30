// Copyright 2019 The Matrix.org Foundation CIC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Seshat - a full text search library for Matrix clients.

#![warn(missing_docs)]

#[cfg(test)]
#[macro_use]
extern crate lazy_static;

mod aesmmapdir;
mod aesstream;
mod database;
mod index;
mod japanese_tokenizer;
mod types;

pub use database::Connection;
pub use database::Database;
pub use database::Searcher;

pub use types::{
    CheckpointDirection, Config, CrawlerCheckpoint, Error, Event, EventType, Language, Profile,
    Result, SearchConfig, SearchResult,
};

pub use std::sync::mpsc::Receiver;

#[cfg(test)]
pub use types::{EVENT, EVENT_SOURCE, TOPIC_EVENT, TOPIC_EVENT_SOURCE};
