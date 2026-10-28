use super::{phys_to_virt, virt_to_phys, Provider};

#[macro_use]
mod log {
    macro_rules! trace {
        ($($arg:expr),*) => { $( let _ = $arg; )* };
    }
    macro_rules! debug {
        ($($arg:expr),*) => { $( let _ = $arg; )* };
    }
    macro_rules! info {
        ($($arg:expr),*) => { $( let _ = $arg; )*};
    }
    macro_rules! warn {
        ($($arg:expr),*) => { $( let _ = $arg; )*};
    }
    macro_rules! error {
        ($($arg:expr),*) => { $( let _ = $arg; )* };
    }
}

// El fichero de registros MII/MDIO genericos vivia aqui, y es el mismo papel
// que `include/uapi/linux/mii.h` en Linux: no tiene nada de Realtek. El GENET
// de la Raspberry Pi 4 lo necesita igual para hablar con su BCM54213PE, asi
// que esta un nivel mas arriba (`net/mii.rs`) y los dos lo comparten. El
// re-export conserva el `super::mii::*` de `rtl8211f.rs` tal cual.
pub use crate::net::mii;
pub mod rtl8211f;
mod utils;
