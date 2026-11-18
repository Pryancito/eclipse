//! `eclipse-btrfs [-f] [-r] DEVICE DIR`: serve the btrfs volume on
//! `DEVICE` at `DIR` (see [`btrfsserve::serve`]).

fn main() {
    btrfsserve::serve::main::<btrfs::Btrfs>("eclipse-btrfs")
}
