use std::io::{Write, BufWriter, Read, Cursor, ErrorKind};
use std::path::{Path};
use std::ops::Deref;
use std::fs::File;
use rand::{Rng, thread_rng};

use crypto::aessafe::{AesSafe128Encryptor, AesSafe128Decryptor};
use crypto::pbkdf2::pbkdf2;
use crypto::sha2::Sha256;
use crypto::hmac::Hmac;
use crypto::aes::{cbc_encryptor, cbc_decryptor, KeySize};
use crypto::blockmodes::PkcsPadding;
use crypto::buffer::{RefReadBuffer, RefWriteBuffer, BufferResult, WriteBuffer, ReadBuffer};

use aesstream::{AesWriter, AesReader};

use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError, OpenDirectoryError};
use tantivy::directory::Directory;
use tantivy::directory::WatchHandle;
use tantivy::directory::{DirectoryLock, Lock, ReadOnlySource, WatchCallback, WritePtr, TerminatingWrite, AntiCallToken};

pub struct AesFile<E: crypto::symmetriccipher::BlockEncryptor, W: Write> (AesWriter<E, W>);

const KEYFILE: &str = "seshat.key";
const SALT_SIZE: usize = 15;
const KEY_SIZE: usize = 16;

impl<E: crypto::symmetriccipher::BlockEncryptor, W: Write> Write for AesFile<E, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<E: crypto::symmetriccipher::BlockEncryptor, W: Write> Drop for AesFile<E, W> {
    fn drop(&mut self) {
        self.flush().expect("Cannot flush thing");
    }
}


impl<E: crypto::symmetriccipher::BlockEncryptor, W: Write> TerminatingWrite for AesFile<E, W> {
    fn terminate_ref(&mut self, _: AntiCallToken) -> std::io::Result<()> {
        Ok(())
    }
}

impl<E: crypto::symmetriccipher::BlockEncryptor, W: Write> Deref for AesFile<E, W> {
    type Target = AesWriter<E, W>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}


#[derive(Clone, Debug)]
pub struct AesMmapDirectory {
    mmap_dir: tantivy::directory::MmapDirectory,
    passphrase: String
}

impl AesMmapDirectory {
    pub fn open<P: AsRef<Path>>(path: P, passphrase: &str) -> Result<Self, OpenDirectoryError> {
        let key_path = path.as_ref().join(KEYFILE);
        let mmap_dir = tantivy::directory::MmapDirectory::open(path)?;

        // TODO make sure to check the password length.

        let key_file = File::open(&key_path);

        let store_key = match key_file {
            Ok(k) => AesMmapDirectory::load_store_key(k, passphrase)?,
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    return Err(e.into());
                }
                AesMmapDirectory::create_new_store(&key_path, passphrase)?
            }
        };

        println!("HELLO KEY {:?}", store_key);

        Ok(AesMmapDirectory { mmap_dir, passphrase: passphrase.to_string() })
    }

    fn load_store_key(mut key_file: File, passphrase: &str) -> Result<Vec<u8>, OpenDirectoryError> {
        let mut iv = [0u8; KEY_SIZE];
        let mut salt = [0u8; SALT_SIZE];
        let mut encrypted_key = vec![];

        key_file.read_exact(&mut iv)?;
        key_file.read_exact(&mut salt)?;
        key_file.read_to_end(&mut encrypted_key)?;

        let derived_key = AesMmapDirectory::rederive_key(passphrase, &salt);
        let mut decryptor = cbc_decryptor(KeySize::KeySize128, &derived_key, &iv, PkcsPadding);
        let mut out = [0u8; KEY_SIZE];
        let mut write_buf = RefWriteBuffer::new(&mut out);

        let remaining;
        let res;
        {
            let mut read_buf = RefReadBuffer::new(&encrypted_key);
            res = decryptor.decrypt(&mut read_buf, &mut write_buf, true).unwrap();
            remaining = read_buf.remaining();
        }

        let len = encrypted_key.len();
        encrypted_key.drain(..(len - remaining));

        match res {
            BufferResult::BufferUnderflow => (),
            BufferResult::BufferOverflow => panic!("HEEEEL"),
        }

        Ok(out.to_vec())
    }

    fn create_new_store(key_path: &Path, passphrase: &str) -> Result<Vec<u8>, OpenDirectoryError> {
        // Derive a AES key from our passphrase using a randomly generated salt
        // to prevent bruteforce attempts using rainbow tables.
        let (derived_key, salt) = AesMmapDirectory::derive_key(passphrase);

        // Generate a random initialization vector for our AES encryptor.
        let iv = AesMmapDirectory::generate_iv();
        // Generate a new random store key. This key will encrypt our tantivy
        // indexing files. The key itself is stored encrypted using the derived
        // key.
        let store_key = AesMmapDirectory::generate_key();
        let mut encryptor = cbc_encryptor(KeySize::KeySize128, &derived_key, &iv, PkcsPadding);

        let mut read_buf = RefReadBuffer::new(&store_key);
        let mut out = [0u8; 1024];
        let mut write_buf = RefWriteBuffer::new(&mut out);
        let mut encrypted_key = Vec::new();

        let mut key_file = File::create(key_path)?;
        // Wrtie down our public salt and iv first, those will be needed to
        // decrypt the key again.
        key_file.write_all(&iv)?;
        key_file.write_all(&salt)?;

        loop {
            let res = encryptor.encrypt(&mut read_buf, &mut write_buf, true).unwrap();
            let mut enc = write_buf.take_read_buffer();
            let mut enc = Vec::from(enc.take_remaining());

            encrypted_key.append(&mut enc);

            match res {
                BufferResult::BufferUnderflow => break,
                _ => panic!("Couldn't encrypt thing")
            }
        }
        key_file.write_all(&encrypted_key)?;

        Ok(store_key)
    }

    fn generate_iv() -> Vec<u8> {
        let mut iv = vec![0u8; KEY_SIZE];
        let mut rng = thread_rng();
        rng.try_fill(&mut iv[..]).unwrap();
        iv
    }

    fn generate_key() -> Vec<u8> {
        let mut key = vec![0u8; KEY_SIZE];
        let mut rng = thread_rng();
        rng.try_fill(&mut key[..]).unwrap();
        key
    }

    fn rederive_key(passphrase: &str, salt: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::new(Sha256::new(), passphrase.as_bytes());
        let mut key = vec![0u8; KEY_SIZE];

        pbkdf2(&mut mac, &salt, KEY_SIZE as u32, &mut key);
        key
    }

    fn derive_key(passphrase: &str) -> (Vec<u8>, Vec<u8>) {
        let mut rng = thread_rng();
        let mut salt = vec![0u8; SALT_SIZE];
        rng.try_fill(&mut salt[..]).unwrap();
        let mut mac = Hmac::new(Sha256::new(), passphrase.as_bytes());
        let mut key = vec![0u8; KEY_SIZE];

        pbkdf2(&mut mac, &salt, KEY_SIZE as u32, &mut key);
        (key, salt)
    }
}

impl Directory for AesMmapDirectory {
    fn open_read(&self, path: &Path) -> Result<ReadOnlySource, OpenReadError> {
        let source = self.mmap_dir.open_read(path)?;

        let decryptor = AesSafe128Decryptor::new(self.passphrase.as_bytes());
        let mut reader = AesReader::new(Cursor::new(source.as_slice()), decryptor).unwrap();
        let mut decrypted = Vec::new();

        reader.read_to_end(&mut decrypted).unwrap();

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
            Err(e) => panic!(e.to_string())
        };

        let encryptor = AesSafe128Encryptor::new(self.passphrase.as_bytes());
        let writer = AesWriter::new(file, encryptor).unwrap();
        let file = AesFile(writer);
        Ok(BufWriter::new(Box::new(file)))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let data = self.mmap_dir.atomic_read(path)?;

        let decryptor = AesSafe128Decryptor::new(self.passphrase.as_bytes());
        let mut reader = AesReader::new(Cursor::new(data), decryptor).unwrap();
        let mut decrypted = Vec::new();

        reader.read_to_end(&mut decrypted).unwrap();
        Ok(decrypted)
    }

    fn atomic_write(&mut self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        let encryptor = AesSafe128Encryptor::new(self.passphrase.as_bytes());
        let mut encrypted = Vec::new();
        {
            let mut writer = AesWriter::new(&mut encrypted, encryptor)?;
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

    let dir = AesMmapDirectory::open(tmpdir.path(), "wordpass").expect("Can't create a new store");
    drop(dir);
    let dir = AesMmapDirectory::open(tmpdir.path(), "wordpass").expect("Can't open the existing store");
    drop(dir);
}
