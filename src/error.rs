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

use fs_extra;
use r2d2;
use tantivy;
use rusqlite;

use failure::Fail;

/// Result type for seshat operations.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Fail, Debug)]
/// Seshat error types.
pub enum Error {
    #[fail(display = "Sqlite pool error: {}", _0)]
    /// Error signaling that there was an error with the Sqlite connection
    /// pool.
    PoolError(r2d2::Error),
    #[fail(display = "Sqlite database error: {}", _0)]
    /// Error signaling that there was an error with a Sqlite transaction.
    DatabaseError(rusqlite::Error),
    #[fail(display = "Index error: {}", _0)]
    /// Error signaling that there was an error with the event indexer.
    IndexError(tantivy::Error),
    #[fail(display = "File system error: {}", _0)]
    /// Error signaling that there was an error while reading from the
    /// filesystem.
    FsError(fs_extra::error::Error),
    /// Error signaling that the database passphrase was incorrect.
    #[fail(display = "Error unlocking the database: {}", _0)]
    DatabaseUnlockError(String),
    /// Error when opening the Seshat database and reading the database version.
    #[fail(display = "Database version missmatch.")]
    DatabaseVersionError,
    /// Error when opening the Seshat database and reading the database version.
    #[fail(display = "Error opening the database: {}", _0)]
    DatabaseOpenError(String),
    /// Error signaling that sqlcipher support is missing.
    #[fail(display = "Sqlcipher error: {}", _0)]
    SqlCipherError(String),
}

impl From<r2d2::Error> for Error {
    fn from(err: r2d2::Error) -> Self {
        Error::PoolError(err)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Error::DatabaseError(err)
    }
}

impl From<tantivy::Error> for Error {
    fn from(err: tantivy::Error) -> Self {
        Error::IndexError(err)
    }
}

impl From<fs_extra::error::Error> for Error {
    fn from(err: fs_extra::error::Error) -> Self {
        Error::FsError(err)
    }
}
