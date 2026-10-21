//! Encryption, the way fscrypt does it (v2 policies).
//!
//! A policy goes on an empty directory and everything created under it
//! inherits it: file contents are encrypted with AES-256-XTS (the tweak is
//! the block's number in the file), names with AES-256-CTS-CBC, each inode
//! with its own key derived from the master key and a random nonce kept in
//! its `ENC_CONTEXT` item. Symlink targets are stored as encrypted file
//! data. Encrypted files are never inline or compressed.
//!
//! Master keys live only in memory ([`add_encryption_key`]). Without the
//! key, names show as base64url of their ciphertext (with a SHA-256 of the
//! rest for long ones), contents cannot be read or written, nothing new can
//! be created, but entries can still be listed, looked up and removed.
//! Checksums cover the ciphertext, so scrub and fsck need no key.
//!
//! [`add_encryption_key`]: CowFs::add_encryption_key

use alloc::borrow::Cow;

use super::*;
use crate::crypt::{self, FileKeys, KEY_ID_SIZE, NONCE_SIZE};

/// `FSCRYPT_MODE_AES_256_XTS` / `FSCRYPT_MODE_AES_256_CTS`.
pub const MODE_AES_256_XTS: u8 = 1;
pub const MODE_AES_256_CTS: u8 = 4;
/// `FSCRYPT_POLICY_FLAGS_PAD_*`: names padded to 4 << (flags & 3) bytes.
pub const POLICY_FLAGS_PAD_MASK: u8 = 0x03;
pub const POLICY_FLAGS_PAD_32: u8 = 0x03;

/// `FSCRYPT_CONTEXT_V2` / `FSCRYPT_POLICY_V2`.
const CONTEXT_V2: u8 = 2;
const CONTEXT_LEN: usize = 40;

/// Ciphertext names up to this length show whole without the key; longer
/// ones as their first [`NOKEY_PREFIX`] bytes and a SHA-256 of the rest.
const NOKEY_WHOLE: usize = 189;
const NOKEY_PREFIX: usize = NOKEY_WHOLE - 32;

/// An encryption policy (`fscrypt_policy_v2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncryptionPolicy {
    pub contents_mode: u8,
    pub filenames_mode: u8,
    pub flags: u8,
    pub key_id: [u8; KEY_ID_SIZE],
}

impl EncryptionPolicy {
    /// The default policy for a master key: AES-256-XTS and AES-256-CTS,
    /// names padded to 32 bytes.
    pub fn new(key_id: [u8; KEY_ID_SIZE]) -> Self {
        Self {
            contents_mode: MODE_AES_256_XTS,
            filenames_mode: MODE_AES_256_CTS,
            flags: POLICY_FLAGS_PAD_32,
            key_id,
        }
    }

    fn supported(&self) -> bool {
        self.contents_mode == MODE_AES_256_XTS
            && self.filenames_mode == MODE_AES_256_CTS
            && self.flags & !POLICY_FLAGS_PAD_MASK == 0
    }

    fn pad(&self) -> usize {
        4 << (self.flags & POLICY_FLAGS_PAD_MASK)
    }
}

/// Policy and nonce of one inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Context {
    policy: EncryptionPolicy,
    nonce: [u8; NONCE_SIZE],
}

impl Context {
    fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; CONTEXT_LEN];
        b[0] = CONTEXT_V2;
        b[1] = self.policy.contents_mode;
        b[2] = self.policy.filenames_mode;
        b[3] = self.policy.flags;
        // b[4]: log2 of the data unit size (0: the block size), b[5..8]
        // reserved.
        b[8..24].copy_from_slice(&self.policy.key_id);
        b[24..40].copy_from_slice(&self.nonce);
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != CONTEXT_LEN || b[0] != CONTEXT_V2 || b[4..8] != [0; 4] {
            return Err(Error::Corrupt("encryption context"));
        }
        let policy = EncryptionPolicy {
            contents_mode: b[1],
            filenames_mode: b[2],
            flags: b[3],
            key_id: b[8..24].try_into().unwrap(),
        };
        if !policy.supported() {
            return Err(Error::Unsupported("encryption mode"));
        }
        Ok(Self {
            policy,
            nonce: b[24..40].try_into().unwrap(),
        })
    }
}

/// A name in an encrypted directory, as the caller gave it.
pub(super) enum NameUse {
    /// To find an existing entry: without the key, a no-key name.
    Find,
    /// To create one: needs the key.
    Create,
}

/// Master keys and the per-inode keys derived from them (memory only).
#[derive(Default)]
pub(super) struct Keyring {
    masters: BTreeMap<[u8; KEY_ID_SIZE], Vec<u8>>,
    /// (subvolume, inode) -> keys, and the master key they come from.
    pub(super) files: BTreeMap<(u64, u64), (Arc<FileKeys>, [u8; KEY_ID_SIZE])>,
}

impl Keyring {
    /// Drop the cached keys of inode `ino` of `subvol` (all of its inodes
    /// for `None`): it is gone, and its number may come back.
    pub(super) fn forget(&mut self, subvol: u64, ino: Option<u64>) {
        if self.files.is_empty() {
            return;
        }
        self.files
            .retain(|&(sv, i), _| sv != subvol || ino.is_some_and(|n| n != i));
    }
}

impl Drop for Keyring {
    fn drop(&mut self) {
        for k in self.masters.values_mut() {
            k.fill(0);
        }
    }
}

impl CowFs {
    /// Random bytes for nonces (the kernel's RNG); without it no encrypted
    /// inode can be created.
    pub fn set_random(&mut self, random: fn(&mut [u8])) {
        self.random = Some(random);
    }

    /// Add a master key (`FS_IOC_ADD_ENCRYPTION_KEY`); returns its
    /// identifier.
    pub fn add_encryption_key(&mut self, key: &[u8]) -> Result<[u8; KEY_ID_SIZE]> {
        if !(crypt::MIN_MASTER_KEY..=crypt::MAX_MASTER_KEY).contains(&key.len()) {
            return Err(Error::Invalid);
        }
        let id = crypt::key_identifier(key);
        self.keyring.masters.insert(id, key.to_vec());
        Ok(id)
    }

    /// Forget a master key and every key derived from it
    /// (`FS_IOC_REMOVE_ENCRYPTION_KEY`).
    pub fn remove_encryption_key(&mut self, id: &[u8; KEY_ID_SIZE]) -> Result<()> {
        let mut k = self.keyring.masters.remove(id).ok_or(Error::NotFound)?;
        k.fill(0);
        self.keyring.files.retain(|_, (_, from)| from != id);
        Ok(())
    }

    /// Whether the master key `id` is present
    /// (`FS_IOC_GET_ENCRYPTION_KEY_STATUS`).
    pub fn encryption_key_present(&self, id: &[u8; KEY_ID_SIZE]) -> bool {
        self.keyring.masters.contains_key(id)
    }

    /// Encrypt directory `h`, which must be empty, with `policy`
    /// (`FS_IOC_SET_ENCRYPTION_POLICY`); its key must have been added.
    /// Setting the policy a directory already has does nothing.
    pub fn set_encryption_policy(&mut self, h: u64, policy: &EncryptionPolicy) -> Result<()> {
        let ino = self.at_rw(h)?;
        self.growable()?;
        if !policy.supported() {
            return Err(Error::Invalid);
        }
        let mut i = self.inode(ino)?;
        if i.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        if let Some(c) = self.context(ino)? {
            return if c.policy == *policy {
                Ok(())
            } else {
                Err(Error::Exists)
            };
        }
        if !self.dir_is_empty(ino)? {
            return Err(Error::NotEmpty);
        }
        if !self.encryption_key_present(&policy.key_id) {
            return Err(Error::NoKey);
        }
        permit(immutable(&i))?;
        let ctx = Context {
            policy: *policy,
            nonce: self.nonce()?,
        };
        self.upsert(Key::new(ino, ENC_CONTEXT, 0), ctx.encode())?;
        i.flags |= FL_ENCRYPT as u64;
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.sb.features |= FEAT_ENCRYPT;
        self.commit()
    }

    /// The policy of `h`, if it is encrypted
    /// (`FS_IOC_GET_ENCRYPTION_POLICY_EX`).
    pub fn encryption_policy(&mut self, h: u64) -> Result<Option<EncryptionPolicy>> {
        let ino = self.at(h)?;
        self.inode(ino)?;
        Ok(self.context(ino)?.map(|c| c.policy))
    }

    /// The nonce of encrypted inode `h` (`FS_IOC_GET_ENCRYPTION_NONCE`).
    pub fn encryption_nonce(&mut self, h: u64) -> Result<[u8; NONCE_SIZE]> {
        let ino = self.at(h)?;
        self.inode(ino)?;
        Ok(self.context(ino)?.ok_or(Error::NotFound)?.nonce)
    }

    fn nonce(&self) -> Result<[u8; NONCE_SIZE]> {
        let random = self
            .random
            .ok_or(Error::Unsupported("no random source for encryption"))?;
        let mut n = [0u8; NONCE_SIZE];
        random(&mut n);
        Ok(n)
    }

    fn context(&mut self, ino: u64) -> Result<Option<Context>> {
        if self.sb.features & FEAT_ENCRYPT == 0 {
            return Ok(None);
        }
        self.get(Key::new(ino, ENC_CONTEXT, 0))?
            .map(|d| Context::decode(&d))
            .transpose()
    }

    /// The keys of `ino` (whose item is `i`): `None` when it is not
    /// encrypted, [`Error::NoKey`] when its master key is missing.
    pub(super) fn file_keys(&mut self, ino: u64, i: &InodeItem) -> Result<Option<Arc<FileKeys>>> {
        if i.flags & FL_ENCRYPT as u64 == 0 {
            return Ok(None);
        }
        if let Some((k, _)) = self.keyring.files.get(&(self.subvol, ino)) {
            return Ok(Some(k.clone()));
        }
        let c = self
            .context(ino)?
            .ok_or(Error::Corrupt("encrypted inode without context"))?;
        let master = self
            .keyring
            .masters
            .get(&c.policy.key_id)
            .ok_or(Error::NoKey)?;
        let k = Arc::new(FileKeys::new(master, &c.nonce));
        self.keyring
            .files
            .insert((self.subvol, ino), (k.clone(), c.policy.key_id));
        Ok(Some(k))
    }

    /// A new inode `ino` (item `i`) in directory `dir` (item `d`) takes
    /// the directory's policy with a nonce of its own.
    pub(super) fn inherit_encryption(
        &mut self,
        dir: u64,
        d: &InodeItem,
        ino: u64,
        i: &mut InodeItem,
    ) -> Result<()> {
        if d.flags & FL_ENCRYPT as u64 == 0 {
            return Ok(());
        }
        // Creating needs the key, as on Linux.
        self.file_keys(dir, d)?;
        let c = self
            .context(dir)?
            .ok_or(Error::Corrupt("encrypted inode without context"))?;
        let ctx = Context {
            policy: c.policy,
            nonce: self.nonce()?,
        };
        self.upsert(Key::new(ino, ENC_CONTEXT, 0), ctx.encode())?;
        i.flags |= FL_ENCRYPT as u64;
        Ok(())
    }

    /// Whether `ino` may get a name in `dir`: an encrypted directory only
    /// takes inodes of its own policy (the key must be there to link).
    pub(super) fn may_link_into(&mut self, dir: u64, ino: u64) -> Result<()> {
        let d = self.inode(dir)?;
        if d.flags & FL_ENCRYPT as u64 == 0 {
            return Ok(());
        }
        self.file_keys(dir, &d)?;
        let theirs = if is_link(ino) {
            None
        } else {
            self.context(ino)?
        };
        let ours = self.context(dir)?.map(|c| c.policy);
        if theirs.map(|c| c.policy) != ours {
            return Err(Error::Unsupported("different encryption policy"));
        }
        Ok(())
    }

    /// The name stored for `name` in `dir`: itself, or for an encrypted
    /// directory its ciphertext (with the key) or what a no-key name stands
    /// for (without, only to find an entry).
    pub(super) fn disk_name<'a>(
        &mut self,
        dir: u64,
        name: &'a str,
        how: NameUse,
    ) -> Result<Cow<'a, [u8]>> {
        let plain = check_name(name)?;
        if self.sb.features & FEAT_ENCRYPT == 0 {
            return Ok(Cow::Borrowed(plain));
        }
        let d = self.inode(dir)?;
        // A plain directory has no keys.
        match self.file_keys(dir, &d) {
            Ok(Some(k)) => {
                let pad = self
                    .context(dir)?
                    .ok_or(Error::Corrupt("encrypted inode without context"))?
                    .policy
                    .pad();
                Ok(Cow::Owned(k.encrypt_name(plain, pad, MAX_NAME_LEN)))
            }
            Ok(None) => Ok(Cow::Borrowed(plain)),
            Err(Error::NoKey) if matches!(how, NameUse::Find) => {
                let ct = crypt::unbase64url(plain).ok_or(Error::NotFound)?;
                if ct.len() < NOKEY_WHOLE {
                    return Ok(Cow::Owned(ct));
                }
                if ct.len() > NOKEY_WHOLE {
                    return Err(Error::NotFound);
                }
                // The prefix and digest of a longer name: no whole one is
                // 189 bytes, as names are padded to a multiple of 4.
                self.find_long_name(dir, &ct).map(Cow::Owned)
            }
            Err(e) => Err(e),
        }
    }

    fn find_long_name(&mut self, dir: u64, nokey: &[u8]) -> Result<Vec<u8>> {
        for (_, d) in self.items_of(dir, DIR_INDEX, usize::MAX)? {
            let e = decode_pair(&d)?;
            if e.name.len() > NOKEY_WHOLE && no_key_name(&e.name) == crypt::base64url(nokey) {
                return Ok(e.name);
            }
        }
        Err(Error::NotFound)
    }

    /// How entries of `dir` are shown: `None` for plain names, else a
    /// function of the stored name.
    pub(super) fn name_shower(&mut self, dir: u64) -> Result<Option<Option<Arc<FileKeys>>>> {
        if self.sb.features & FEAT_ENCRYPT == 0 {
            return Ok(None);
        }
        let d = self.inode(dir)?;
        if d.flags & FL_ENCRYPT as u64 == 0 {
            return Ok(None);
        }
        match self.file_keys(dir, &d) {
            Ok(k) => Ok(Some(k)),
            Err(Error::NoKey) => Ok(Some(None)),
            Err(e) => Err(e),
        }
    }

    /// Refuse what cannot work on an encrypted inode (sharing extents
    /// between files with different keys, sending plaintext).
    pub(super) fn refuse_encrypted(&self, i: &InodeItem) -> Result<()> {
        if i.flags & FL_ENCRYPT as u64 != 0 {
            return Err(Error::Unsupported("encrypted file"));
        }
        Ok(())
    }
}

/// fsck: every encrypted inode has a valid context and no other inode has
/// one; encrypted files keep no inline or compressed data; an encrypted
/// directory names only inodes of its own policy, by names at least one
/// cipher block long, and no subvolumes.
pub(super) fn check(
    inodes: &BTreeMap<u64, InodeItem>,
    contexts: &BTreeMap<u64, Vec<u8>>,
    packed: &BTreeSet<u64>,
    entries: &BTreeSet<(u64, u64, Vec<u8>, u64)>,
) -> Result<()> {
    let mut policies = BTreeMap::new();
    for (ino, raw) in contexts {
        let c = Context::decode(raw).map_err(|_| Error::Corrupt("fsck: encryption context"))?;
        policies.insert(*ino, c.policy);
    }
    for (ino, i) in inodes {
        let enc = i.flags & FL_ENCRYPT as u64 != 0;
        if enc != policies.contains_key(ino) {
            return Err(Error::Corrupt("fsck: encryption flag and context differ"));
        }
        if enc && packed.contains(ino) {
            return Err(Error::Corrupt("fsck: inline or compressed encrypted data"));
        }
    }
    if contexts.keys().any(|i| !inodes.contains_key(i)) {
        return Err(Error::Corrupt(
            "fsck: encryption context of a missing inode",
        ));
    }
    for (dir, _, name, target) in entries {
        let Some(p) = policies.get(dir) else {
            continue;
        };
        if is_link(*target) || policies.get(target) != Some(p) || name.len() < 16 {
            return Err(Error::Corrupt("fsck: entry of an encrypted directory"));
        }
    }
    Ok(())
}

/// The name shown for stored name `ct` of an encrypted directory: the
/// plaintext with the key, else the no-key name.
pub(super) fn shown_name(keys: Option<&FileKeys>, ct: &[u8]) -> Result<Vec<u8>> {
    match keys {
        Some(k) => k
            .decrypt_name(ct)
            .filter(|n| !n.is_empty())
            .ok_or(Error::Corrupt("encrypted name")),
        None => Ok(no_key_name(ct)),
    }
}

fn no_key_name(ct: &[u8]) -> Vec<u8> {
    if ct.len() <= NOKEY_WHOLE {
        return crypt::base64url(ct);
    }
    let mut v = ct[..NOKEY_PREFIX].to_vec();
    v.extend_from_slice(&crypt::sha256(&ct[NOKEY_PREFIX..]));
    crypt::base64url(&v)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_is_xts_cts_padded_to_32() {
        let p = EncryptionPolicy::new([9; KEY_ID_SIZE]);
        assert!(p.supported());
        assert_eq!(p.pad(), 32);
        assert_eq!(p.key_id, [9; KEY_ID_SIZE]);
    }

    #[test]
    fn the_padding_follows_the_flags() {
        let mut p = EncryptionPolicy::new([0; KEY_ID_SIZE]);
        for (flags, pad) in [(0u8, 4usize), (1, 8), (2, 16), (3, 32)] {
            p.flags = flags;
            assert_eq!(p.pad(), pad);
        }
    }

    /// Only the modes this crate implements are accepted: a context naming
    /// another one must not be read with the wrong cipher.
    #[test]
    fn other_modes_and_flags_are_unsupported() {
        let ok = EncryptionPolicy::new([0; KEY_ID_SIZE]);
        assert!(!EncryptionPolicy {
            contents_mode: 9,
            ..ok
        }
        .supported());
        assert!(!EncryptionPolicy {
            filenames_mode: 1,
            ..ok
        }
        .supported());
        assert!(!EncryptionPolicy { flags: 0x04, ..ok }.supported());
    }

    /// The context is the 40-byte `fscrypt_context_v2`, and decodes back to
    /// itself.
    #[test]
    fn a_context_round_trips_in_the_fscrypt_layout() {
        let c = Context {
            policy: EncryptionPolicy::new(*b"0123456789abcdef"),
            nonce: *b"fedcba9876543210",
        };
        let b = c.encode();
        assert_eq!(b.len(), CONTEXT_LEN);
        assert_eq!(b[0], CONTEXT_V2);
        assert_eq!(
            (b[1], b[2], b[3]),
            (MODE_AES_256_XTS, MODE_AES_256_CTS, POLICY_FLAGS_PAD_32)
        );
        assert_eq!(&b[4..8], &[0; 4]);
        assert_eq!(&b[8..24], b"0123456789abcdef");
        assert_eq!(&b[24..40], b"fedcba9876543210");
        assert_eq!(Context::decode(&b).unwrap(), c);
    }

    #[test]
    fn a_bad_context_is_refused() {
        let good = Context {
            policy: EncryptionPolicy::new([1; KEY_ID_SIZE]),
            nonce: [2; NONCE_SIZE],
        }
        .encode();
        assert!(matches!(
            Context::decode(&good[..39]),
            Err(Error::Corrupt(_))
        ));
        let mut b = good.clone();
        b.push(0);
        assert!(matches!(Context::decode(&b), Err(Error::Corrupt(_))));
        let mut b = good.clone();
        b[0] = 1;
        assert!(matches!(Context::decode(&b), Err(Error::Corrupt(_))));
        let mut b = good.clone();
        b[5] = 1;
        assert!(matches!(Context::decode(&b), Err(Error::Corrupt(_))));
        let mut b = good;
        b[1] = 2;
        assert!(matches!(Context::decode(&b), Err(Error::Unsupported(_))));
    }

    fn keyring_with(entries: &[(u64, u64)]) -> Keyring {
        let mut k = Keyring::default();
        for &(sv, ino) in entries {
            let keys = Arc::new(FileKeys::new(&[7u8; 64], &[0; NONCE_SIZE]));
            k.files.insert((sv, ino), (keys, [0; KEY_ID_SIZE]));
        }
        k
    }

    /// `forget` drops one inode's keys, or all of one subvolume's, and never
    /// another subvolume's inode that happens to have the same number.
    #[test]
    fn forget_drops_only_what_it_names() {
        let mut k = keyring_with(&[(5, 300), (5, 301), (6, 300)]);
        k.forget(5, Some(300));
        assert_eq!(
            k.files.keys().copied().collect::<Vec<_>>(),
            vec![(5, 301), (6, 300)]
        );
        let mut k = keyring_with(&[(5, 300), (5, 301), (6, 300)]);
        k.forget(5, None);
        assert_eq!(k.files.keys().copied().collect::<Vec<_>>(), vec![(6, 300)]);
        let mut k = Keyring::default();
        k.forget(5, None);
        assert!(k.files.is_empty());
    }
}
