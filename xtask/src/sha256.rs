//! SHA-256 for the payloads the installer verifies before writing them.
//!
//! `install-eclipse` looks for `<payload>.sha256` next to each of
//! `efi.img.gz`, `rootfs.btrfs.gz` and `home.btrfs.gz` and checks the image
//! against it before touching the disk. Nothing ever wrote those files, so
//! the check printed "No existe boot/efi.img.gz.sha256; se omite verificación
//! SHA-256 previa" on every single install and then wrote a payload nobody
//! had looked at -- a truncated download or a half-copied USB stick reached
//! the disk as if it were fine.
//!
//! This is written out by hand rather than pulled in as a dependency: it is
//! sixty lines, it runs once per image build over three files, and the
//! installer carries its own copy of the same algorithm for the same reason.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// A streaming SHA-256: the payloads are gigabyte-sized, so they are hashed
/// as they are read rather than loaded whole.
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    filled: usize,
    len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            filled: 0,
            len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                let block = self.block;
                self.compress(&block);
                self.filled = 0;
            }
        }
    }

    /// The digest as lower-case hex, which is what `install-eclipse` reads.
    pub fn hex(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        // The length goes in the last 8 bytes of a block, so pad until there
        // is room for it -- a block that is already past that point gets a
        // whole extra one.
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.len = 0; // `update` must not count the length field itself.
        self.update(&bits.to_be_bytes());
        debug_assert_eq!(self.filled, 0);

        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// Hex digest of a file, read in 1 MiB chunks.
pub fn hex_of_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.hex())
}

/// Write `<path>.sha256` in `sha256sum` format next to `path`.
///
/// The installer only reads the first field, but the file is also the one a
/// person checks by hand with `sha256sum -c`, so it carries the name too.
pub fn write_sidecar(path: &std::path::Path) -> std::io::Result<()> {
    let hex = hex_of_file(path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".sha256");
    std::fs::write(
        std::path::PathBuf::from(sidecar),
        format!("{hex}  {name}\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        h.hex()
    }

    #[test]
    fn the_published_vectors() {
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_length_that_lands_on_the_padding_boundary() {
        // 55, 56 and 64 bytes: the sizes where the length field does or does
        // not fit in the last block, which is where a hand-written padding
        // loop goes wrong.
        for len in [55usize, 56, 57, 63, 64, 65] {
            let data = vec![b'a'; len];
            let mut split = Sha256::new();
            // The same bytes fed in two pieces must hash the same, which is
            // what the streaming path over a gigabyte file depends on.
            split.update(&data[..len / 2]);
            split.update(&data[len / 2..]);
            assert_eq!(split.hex(), hex(&data), "len {len}");
        }
    }

    #[test]
    fn a_payload_sized_stream_in_uneven_pieces() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut streamed = Sha256::new();
        let mut at = 0;
        for piece in [1usize, 63, 64, 65, 1000, 4096].into_iter().cycle() {
            if at >= data.len() {
                break;
            }
            let end = (at + piece).min(data.len());
            streamed.update(&data[at..end]);
            at = end;
        }
        assert_eq!(streamed.hex(), hex(&data));
    }

    #[test]
    fn the_digest_of_a_file_is_the_digest_of_its_bytes() {
        // `hex_of_file` reads in 1 MiB chunks, so the sizes that matter are
        // the ones around a chunk boundary: the last read is the short one,
        // and hashing the whole buffer instead of the bytes it filled puts a
        // megabyte of stale zeros into the digest of every file whose size is
        // not a multiple of the chunk, which is every payload the installer
        // verifies.
        let dir = std::env::temp_dir();
        for len in [0usize, 1, 4095, 1 << 20, (1 << 20) + 1, (3 << 20) + 7] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let path = dir.join(format!("xtask-sha256-{}-{len}.bin", std::process::id()));
            std::fs::write(&path, &data).unwrap();
            assert_eq!(hex_of_file(&path).unwrap(), hex(&data), "len {len}");
            std::fs::remove_file(&path).unwrap();
        }
        // A file that is not there is an error, not the digest of nothing:
        // the installer tells the two apart to decide whether to go on.
        assert!(hex_of_file(&dir.join("xtask-sha256-no-existe")).is_err());
    }

    #[test]
    fn the_sidecar_is_the_line_sha256sum_reads_back() {
        // `install-eclipse` reads the first field and a person reads the whole
        // line with `sha256sum -c`, which wants exactly two spaces, the BARE
        // file name and a trailing newline. Nothing ever read this file back,
        // so all four could be wrong at once -- and a sidecar under the wrong
        // NAME is the original bug: the installer does not find it and skips
        // the check it was told to make.
        let dir = std::env::temp_dir().join(format!("xtask-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let payload = dir.join("rootfs.btrfs.gz");
        let bytes = b"not really a payload";
        std::fs::write(&payload, bytes).unwrap();
        write_sidecar(&payload).unwrap();

        let side = dir.join("rootfs.btrfs.gz.sha256");
        assert!(side.exists(), "el instalador busca exactamente ese nombre");
        let text = std::fs::read_to_string(&side).unwrap();
        assert_eq!(text, format!("{}  rootfs.btrfs.gz\n", hex(bytes)));
        // And each half of that line on its own, so a failure says which of
        // the two moved.
        let (digest, rest) = text.split_once("  ").expect("hacen falta DOS espacios");
        assert_eq!(digest, hex(bytes));
        assert_eq!(rest, "rootfs.btrfs.gz\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
