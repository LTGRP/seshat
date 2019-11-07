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

use rand::{thread_rng, Rng};
use std::fs::File;
use std::io::Error as IoError;
use std::io::{BufWriter, Cursor, ErrorKind, Read, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use crypto::aessafe::AesSafe256Encryptor;
use crypto::blockmodes::CtrMode;
use crypto::buffer::{BufferResult, ReadBuffer, RefReadBuffer, RefWriteBuffer, WriteBuffer};
use crypto::hkdf::hkdf_expand;
use crypto::hmac::Hmac;
use crypto::mac::{Mac, MacResult};
use crypto::pbkdf2::pbkdf2;
use crypto::sha2::{Sha256, Sha512};
use crypto::symmetriccipher::{Decryptor, Encryptor};

use crate::aesstream::{AesReader, AesWriter};

use tantivy::directory::error::IOError as TvIoError;
use tantivy::directory::error::{
    DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError,
};
use tantivy::directory::Directory;
use tantivy::directory::WatchHandle;
use tantivy::directory::{
    AntiCallToken, DirectoryLock, Lock, ReadOnlySource, TerminatingWrite, WatchCallback, WritePtr,
};

pub struct AesFile<E: crypto::symmetriccipher::BlockEncryptor, M: Mac, W: Write>(
    AesWriter<E, M, W>,
);

type KeyDerivationResult = (Vec<u8>, Vec<u8>, Vec<u8>);

const KEYFILE: &str = "seshat-index.key";
const SALT_SIZE: usize = 16;
const IV_SIZE: usize = 16;
const KEY_SIZE: usize = 32;
const MAC_LENGTH: usize = 32;
const VERSION: u8 = 1;

#[cfg(test)]
const PBKDF_COUNT: u32 = 10;

#[cfg(not(test))]
const PBKDF_COUNT: u32 = 10_000;

impl<E: crypto::symmetriccipher::BlockEncryptor, M: Mac, W: Write> Write for AesFile<E, M, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<E: crypto::symmetriccipher::BlockEncryptor, M: Mac, W: Write> TerminatingWrite
    for AesFile<E, M, W>
{
    fn terminate_ref(&mut self, _: AntiCallToken) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<E: crypto::symmetriccipher::BlockEncryptor, M: Mac, W: Write> Deref for AesFile<E, M, W> {
    type Target = AesWriter<E, M, W>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct EncryptedMmapDirectory {
    path: PathBuf,
    mmap_dir: tantivy::directory::MmapDirectory,
    encryption_key: Vec<u8>,
    mac_key: Vec<u8>,
}

impl EncryptedMmapDirectory {
    pub fn open<P: AsRef<Path>>(path: P, passphrase: &str) -> Result<Self, OpenDirectoryError> {
        let path = PathBuf::from(path.as_ref());

        let key_path = path.as_path().join(KEYFILE);
        let mmap_dir = tantivy::directory::MmapDirectory::open(&path)?;

        if passphrase.is_empty() {
            return Err(IoError::new(ErrorKind::Other, "empty passphrase").into());
        }

        let key_file = File::open(&key_path);

        let store_key = match key_file {
            Ok(k) => EncryptedMmapDirectory::load_store_key(k, passphrase)?,
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    return Err(e.into());
                }
                EncryptedMmapDirectory::create_new_store(&key_path, passphrase)?
            }
        };

        let (encryption_key, mac_key) = EncryptedMmapDirectory::expand_store_key(&store_key);

        Ok(EncryptedMmapDirectory {
            path,
            mmap_dir,
            encryption_key,
            mac_key,
        })
    }

    pub fn change_passphrase(&self, old: &str, new: &str) -> Result<(), OpenDirectoryError>{
        if old.is_empty() || new.is_empty() {
            return Err(IoError::new(ErrorKind::Other, "empty passphrase").into());
        }

        let key_path = self.path.join(KEYFILE);
        let key_file = File::open(&key_path)?;

        // Load our store key using the old passphrase.
        let store_key = EncryptedMmapDirectory::load_store_key(key_file, old)?;
        // Derive new encryption keys using the new passphrase.
        let (key, hmac_key, salt) = EncryptedMmapDirectory::derive_key(new)?;
        // Re-encrypt our store key using the newly derived keys.
        EncryptedMmapDirectory::encrypt_store_key(&key, &salt, &hmac_key, &store_key, &key_path)?;

        Ok(())
    }

    fn expand_store_key(store_key: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut hkdf_result = [0u8; KEY_SIZE * 2];

        hkdf_expand(Sha512::new(), &store_key, &[], &mut hkdf_result);
        let (key, hmac_key) = hkdf_result.split_at(KEY_SIZE);
        (Vec::from(key), Vec::from(hmac_key))
    }

    fn load_store_key(mut key_file: File, passphrase: &str) -> Result<Vec<u8>, OpenDirectoryError> {
        let mut iv = [0u8; IV_SIZE];
        let mut salt = [0u8; SALT_SIZE];
        let mut expected_mac = [0u8; MAC_LENGTH];
        let mut version = [0u8; 1];
        let mut encrypted_key = vec![];

        // Read our iv, salt and encrypted key from our key file.
        key_file.read_exact(&mut version)?;
        key_file.read_exact(&mut iv)?;
        key_file.read_exact(&mut salt)?;
        key_file.read_exact(&mut expected_mac)?;
        key_file.read_to_end(&mut encrypted_key)?;

        if version[0] != VERSION {
            return Err(IoError::new(ErrorKind::Other, "invalid index store version").into());
        }

        // Rederive our key using the passphrase and salt.
        let (key, hmac_key) = EncryptedMmapDirectory::rederive_key(passphrase, &salt);

        let expected_mac = MacResult::new(&expected_mac);
        let mac = EncryptedMmapDirectory::calculate_hmac(
            version[0],
            &iv,
            &salt,
            &encrypted_key,
            &hmac_key,
        );

        if mac != expected_mac {
            return Err(IoError::new(ErrorKind::Other, "invalid MAC of the store key").into());
        }

        let algorithm = AesSafe256Encryptor::new(&key);
        let mut decryptor = CtrMode::new(algorithm, iv.to_vec());

        let mut out = [0u8; KEY_SIZE];
        let mut write_buf = RefWriteBuffer::new(&mut out);

        let remaining;
        // Decrypt the encrypted key and return it.
        let res;
        {
            let mut read_buf = RefReadBuffer::new(&encrypted_key);
            res = decryptor
                .decrypt(&mut read_buf, &mut write_buf, true)
                .map_err(|e| {
                    IoError::new(
                        ErrorKind::Other,
                        format!("error decrypting store key: {:?}", e),
                    )
                })?;
            remaining = read_buf.remaining();
        }

        let len = encrypted_key.len();
        encrypted_key.drain(..(len - remaining));

        match res {
            BufferResult::BufferUnderflow => (),
            BufferResult::BufferOverflow => {
                return Err(IoError::new(ErrorKind::Other, "error decrypting store key").into())
            }
        }

        Ok(out.to_vec())
    }

    fn calculate_hmac(
        version: u8,
        iv: &[u8],
        salt: &[u8],
        encrypted_key: &[u8],
        key: &[u8],
    ) -> MacResult {
        let mut hmac = Hmac::new(Sha256::new(), key);
        hmac.input(&[version]);
        hmac.input(&iv);
        hmac.input(&salt);
        hmac.input(&encrypted_key);
        hmac.result()
    }

    fn create_new_store(key_path: &Path, passphrase: &str) -> Result<Vec<u8>, OpenDirectoryError> {
        // Derive a AES key from our passphrase using a randomly generated salt
        // to prevent bruteforce attempts using rainbow tables.
        let (key, hmac_key, salt) = EncryptedMmapDirectory::derive_key(passphrase)?;
        // Generate a new random store key. This key will encrypt our tantivy
        // indexing files. The key itself is stored encrypted using the derived
        // key.
        let store_key = EncryptedMmapDirectory::generate_key()?;

        // Encrypt and save the encrypted store key to a file.
        EncryptedMmapDirectory::encrypt_store_key(&key, &salt, &hmac_key, &store_key, key_path)?;

        Ok(store_key)
    }

    fn encrypt_store_key(key: &[u8], salt: &[u8], hmac_key: &[u8], store_key: &[u8], key_path: &Path) -> Result<(), OpenDirectoryError>{
        // Generate a random initialization vector for our AES encryptor.
        let iv = EncryptedMmapDirectory::generate_iv()?;
        let algorithm = AesSafe256Encryptor::new(&key);
        let mut encryptor = CtrMode::new(algorithm, iv.clone());

        let mut read_buf = RefReadBuffer::new(&store_key);
        let mut out = [0u8; 1024];
        let mut write_buf = RefWriteBuffer::new(&mut out);
        let mut encrypted_key = Vec::new();

        let mut key_file = File::create(key_path)?;

        // Write down our public salt and iv first, those will be needed to
        // decrypt the key again.
        key_file.write_all(&[VERSION])?;
        key_file.write_all(&iv)?;
        key_file.write_all(&salt)?;

        // Encrypt our key.
        loop {
            let res = encryptor
                .encrypt(&mut read_buf, &mut write_buf, true)
                .map_err(|e| {
                    IoError::new(
                        ErrorKind::Other,
                        format!("unable to encrypt store key: {:?}", e),
                    )
                })?;
            let mut enc = write_buf.take_read_buffer();
            let mut enc = Vec::from(enc.take_remaining());

            encrypted_key.append(&mut enc);

            match res {
                BufferResult::BufferUnderflow => break,
                _ => panic!("Couldn't encrypt the store key"),
            }
        }

        let mac =
            EncryptedMmapDirectory::calculate_hmac(VERSION, &iv, &salt, &encrypted_key, &hmac_key);
        key_file.write_all(mac.code())?;

        // Write down the encrypted key.
        key_file.write_all(&encrypted_key)?;

        Ok(())
    }

    fn generate_iv() -> Result<Vec<u8>, OpenDirectoryError> {
        let mut iv = vec![0u8; IV_SIZE];
        let mut rng = thread_rng();
        rng.try_fill(&mut iv[..])
            .map_err(|e| IoError::new(ErrorKind::Other, format!("error generating iv: {:?}", e)))?;
        Ok(iv)
    }

    fn generate_key() -> Result<Vec<u8>, OpenDirectoryError> {
        let mut key = vec![0u8; KEY_SIZE];
        let mut rng = thread_rng();
        rng.try_fill(&mut key[..]).map_err(|e| {
            IoError::new(ErrorKind::Other, format!("error generating key: {:?}", e))
        })?;
        Ok(key)
    }

    fn rederive_key(passphrase: &str, salt: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut mac = Hmac::new(Sha512::new(), passphrase.as_bytes());
        let mut pbkdf_result = [0u8; KEY_SIZE * 2];

        pbkdf2(&mut mac, &salt, PBKDF_COUNT, &mut pbkdf_result);
        let (key, hmac_key) = pbkdf_result.split_at(KEY_SIZE);
        (Vec::from(key), Vec::from(hmac_key))
    }

    fn derive_key(passphrase: &str) -> Result<KeyDerivationResult, OpenDirectoryError> {
        let mut rng = thread_rng();
        let mut salt = vec![0u8; SALT_SIZE];
        rng.try_fill(&mut salt[..]).map_err(|e| {
            IoError::new(ErrorKind::Other, format!("error generating salt: {:?}", e))
        })?;

        let (key, hmac_key) = EncryptedMmapDirectory::rederive_key(passphrase, &salt);
        Ok((key, hmac_key, salt))
    }
}

impl Directory for EncryptedMmapDirectory {
    fn open_read(&self, path: &Path) -> Result<ReadOnlySource, OpenReadError> {
        let source = self.mmap_dir.open_read(path)?;

        let decryptor = AesSafe256Encryptor::new(&self.encryption_key);
        let mac = Hmac::new(Sha256::new(), &self.mac_key);
        let mut reader = AesReader::new(Cursor::new(source.as_slice()), decryptor, mac)
            .map_err(TvIoError::from)?;
        let mut decrypted = Vec::new();

        reader
            .read_to_end(&mut decrypted)
            .map_err(TvIoError::from)?;

        Ok(ReadOnlySource::from(decrypted))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.mmap_dir.delete(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.mmap_dir.exists(path)
    }

    fn open_write(&mut self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let file = match self.mmap_dir.open_write(path)?.into_inner() {
            Ok(f) => f,
            Err(e) => panic!(e.to_string()),
        };

        let encryptor = AesSafe256Encryptor::new(&self.encryption_key);
        let mac = Hmac::new(Sha256::new(), &self.mac_key);
        let writer = AesWriter::new(file, encryptor, mac).map_err(TvIoError::from)?;
        let file = AesFile(writer);
        Ok(BufWriter::new(Box::new(file)))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let data = self.mmap_dir.atomic_read(path)?;

        let decryptor = AesSafe256Encryptor::new(&self.encryption_key);
        let mac = Hmac::new(Sha256::new(), &self.mac_key);
        let mut reader =
            AesReader::new(Cursor::new(data), decryptor, mac).map_err(TvIoError::from)?;
        let mut decrypted = Vec::new();

        reader
            .read_to_end(&mut decrypted)
            .map_err(TvIoError::from)?;
        Ok(decrypted)
    }

    fn atomic_write(&mut self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        let encryptor = AesSafe256Encryptor::new(&self.encryption_key);
        let mac = Hmac::new(Sha256::new(), &self.mac_key);
        let mut encrypted = Vec::new();
        {
            let mut writer = AesWriter::new(&mut encrypted, encryptor, mac)?;
            writer.write_all(data)?;
        }

        self.mmap_dir.atomic_write(path, &encrypted)
    }

    fn watch(&self, watch_callback: WatchCallback) -> Result<WatchHandle, tantivy::Error> {
        self.mmap_dir.watch(watch_callback)
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        self.mmap_dir.acquire_lock(lock)
    }
}

#[cfg(test)]
use tempfile::tempdir;

#[test]
fn create_new_store_and_reopen() {
    let tmpdir = tempdir().unwrap();
    let dir =
        EncryptedMmapDirectory::open(tmpdir.path(), "wordpass").expect("Can't create a new store");
    drop(dir);
    let dir = EncryptedMmapDirectory::open(tmpdir.path(), "wordpass")
        .expect("Can't open the existing store");
    drop(dir);
    let dir = EncryptedMmapDirectory::open(tmpdir.path(), "password");
    assert!(
        dir.is_err(),
        "Opened an existing store with the wrong passphrase"
    );
}

#[test]
fn create_store_with_empty_passphrase() {
    let tmpdir = tempdir().unwrap();
    let dir = EncryptedMmapDirectory::open(tmpdir.path(), "");
    assert!(
        dir.is_err(),
        "Opened an existing store with the wrong passphrase"
    );
}

#[test]
fn change_passphrase() {
    let tmpdir = tempdir().unwrap();
    let dir =
        EncryptedMmapDirectory::open(tmpdir.path(), "wordpass").expect("Can't create a new store");

    dir.change_passphrase("wordpass", "password").expect("Can't change passphrase");
    drop(dir);
    let dir = EncryptedMmapDirectory::open(tmpdir.path(), "wordpass");
    assert!(
        dir.is_err(),
        "Opened an existing store with the old passphrase"
    );
    let _ = EncryptedMmapDirectory::open(tmpdir.path(), "password")
        .expect("Can't open the store with the new passphrase");
}
