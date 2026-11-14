//! Whose the machine's network is, as its boot says.
//!
//! The first virtio network card is left to userspace, by the kernel
//! itself or because the boot says so (`udriver=virtio-net@0`:
//! `zcore_drivers::user`), and goes to the stack that runs as a process,
//! which is the card's driver too. That stack is the machine's network:
//! every program's sockets are its own, asked for or not. That is what a
//! machine with such a card gets by saying nothing, and these are the
//! words that change it:
//!
//! * `unetstack=kernel`: the card goes to the kernel's own stack, with a
//!   driver process to carry its frames, and no stack runs as a process.
//! * `unetstack=none`: the card is not left to userspace unless the boot
//!   names it, and no stack takes it either way.
//! * `unetuse=asked`: the stack process has the card, and only the
//!   programs started by `eclipse-netstack-use` have its sockets.
//! * `unetuse=all`: every program's sockets are a stack process's whatever
//!   else the boot says, even while none runs.
//!
//! A machine where the kernel drives a card of its own keeps the kernel's
//! stack for the programs that do not ask, since that stack is the only one
//! that reaches that card: there the stack process is for the ones that do,
//! unless the boot says `unetuse=all`.

/// The card the stack process takes, as `udriver=` names it and as
/// `zcore_drivers::user::reserved` lists it: the one the `unet` boot
/// service is started for.
pub const CARD: &str = "virtio-net@0";

/// Whose the sockets are of a program that did not ask for the stack that
/// runs as a process, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    /// The stack process's: the boot says so (`unetuse=all`).
    AllSaid,
    /// The stack process's: it has the only card of the machine.
    AllOnlyCard,
    /// The kernel's: the boot says so (`unetuse=asked`).
    AskedSaid,
    /// The kernel's: this boot gives no card to a stack process.
    NoStack,
    /// The kernel's: it drives this many cards of its own.
    KernelCards(usize),
}

impl Whose {
    /// Whether every program's sockets are the stack process's.
    pub fn all(self) -> bool {
        matches!(self, Whose::AllSaid | Whose::AllOnlyCard)
    }
}

/// Whether a stack that runs as a process has a card on this boot: `card`
/// is whether [`CARD`] is there and left to userspace, and `unetstack` the
/// value of that word on the command line.
pub fn has_stack(card: bool, unetstack: Option<&str>) -> bool {
    card && !matches!(unetstack, Some("kernel") | Some("none"))
}

/// What `unetuse=` can say. Any other word is as good as none.
pub fn understood(unetuse: &str) -> bool {
    matches!(unetuse, "all" | "asked")
}

/// Whose the sockets are on a boot where [`CARD`] is left to userspace or
/// not (`card`), whose command line has these values of `unetstack=` and
/// `unetuse=` (`None` for a word that is not there), on a machine where
/// the kernel drives `kernel_cards` network cards itself, the loopback not
/// counted.
pub fn whose(
    card: bool,
    unetstack: Option<&str>,
    unetuse: Option<&str>,
    kernel_cards: usize,
) -> Whose {
    match unetuse {
        Some("all") => Whose::AllSaid,
        Some("asked") => Whose::AskedSaid,
        _ if !has_stack(card, unetstack) => Whose::NoStack,
        _ if kernel_cards > 0 => Whose::KernelCards(kernel_cards),
        _ => Whose::AllOnlyCard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stack_process_is_the_network_of_a_machine_whose_only_card_it_has() {
        // Nothing but the card left to userspace: the stack process takes
        // it, and every program has that stack.
        assert_eq!(whose(true, None, None, 0), Whose::AllOnlyCard);
        // With an address of its own or a server's, the same.
        assert_eq!(whose(true, Some("dhcp"), None, 0), Whose::AllOnlyCard);
        assert_eq!(
            whose(true, Some("10.0.9.15/24,10.0.9.2"), None, 0),
            Whose::AllOnlyCard
        );
        assert!(Whose::AllOnlyCard.all());
    }

    #[test]
    fn a_boot_that_leaves_no_card_to_a_stack_process_keeps_the_kernels() {
        // A machine without such a card, or a boot that did not leave it.
        assert_eq!(whose(false, None, None, 1), Whose::NoStack);
        assert_eq!(whose(false, None, None, 0), Whose::NoStack);
        // The card left to userspace for the kernel's own stack, or for
        // nobody's.
        assert_eq!(whose(true, Some("kernel"), None, 0), Whose::NoStack);
        assert_eq!(whose(true, Some("none"), None, 0), Whose::NoStack);
        // An address there does not make a stack of it.
        assert_eq!(whose(false, Some("dhcp"), None, 0), Whose::NoStack);
        assert!(!Whose::NoStack.all());
    }

    #[test]
    fn a_card_of_the_kernels_keeps_its_stack_for_who_does_not_ask() {
        assert_eq!(whose(true, None, None, 1), Whose::KernelCards(1));
        assert_eq!(whose(true, Some("dhcp"), None, 2), Whose::KernelCards(2));
        assert!(!Whose::KernelCards(1).all());
    }

    #[test]
    fn what_the_boot_says_of_every_program_is_what_counts() {
        // `unetuse=all`: with a card of the kernel's, and even with no
        // stack process of the boot's (one can be run by hand).
        assert_eq!(whose(true, None, Some("all"), 1), Whose::AllSaid);
        assert_eq!(whose(false, None, Some("all"), 1), Whose::AllSaid);
        assert_eq!(whose(true, Some("kernel"), Some("all"), 0), Whose::AllSaid);
        assert!(Whose::AllSaid.all());
        // `unetuse=asked`: the stack has the only card and still only the
        // programs that ask have it.
        assert_eq!(whose(true, None, Some("asked"), 0), Whose::AskedSaid);
        assert_eq!(
            whose(true, Some("dhcp"), Some("asked"), 0),
            Whose::AskedSaid
        );
        assert!(!Whose::AskedSaid.all());
        // A word that is neither is as good as none.
        assert!(understood("all") && understood("asked"));
        assert!(!understood("") && !understood("ALL") && !understood("every"));
        assert_eq!(whose(true, None, Some("every"), 0), Whose::AllOnlyCard);
        assert_eq!(whose(true, None, Some("every"), 1), Whose::KernelCards(1));
        assert_eq!(whose(false, None, Some(""), 0), Whose::NoStack);
    }
}
