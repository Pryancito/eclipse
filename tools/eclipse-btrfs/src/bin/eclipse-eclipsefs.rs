//! `eclipse-eclipsefs [-f] [-r] DEVICE DIR`: serve the EclipseFS volume on
//! `DEVICE` at `DIR` (see [`btrfsserve::serve`]).

fn main() {
    btrfsserve::serve::main::<eclipsefs::EclipseFS>("eclipse-eclipsefs")
}
