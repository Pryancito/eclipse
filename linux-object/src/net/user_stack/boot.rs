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
//!
//! `unetcard=kernel` is the boot that gives the stack process the kernel's
//! own first card instead: the kernel goes on driving it and lends it
//! ([`lends`]), its frames go to the stack process and not to the kernel's
//! stack, and it counts as the stack process's card in all of the above.
//! It is for a machine with no virtio network card left to userspace, which
//! would be that stack's card first.

/// The card the stack process takes, as `udriver=` names it and as
/// `zcore_drivers::user::reserved` lists it: the one the `unet` boot
/// service is started for.
pub const CARD: &str = "virtio-net@0";

/// The kernel's first card when it is lent to the stack process, as
/// `/proc/udrivers` lists it: the one the `knet` boot service is started
/// for.
pub const LENT_CARD: &str = "kernel-net@0";

static LENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether this boot lends the kernel's first card to a stack that runs as
/// a process (`unetcard=kernel`): `unetcard` and `unetstack` are the values
/// of those words, `card` is whether [`CARD`] is there and left to
/// userspace, and `lendable` whether the kernel drives a first card and its
/// driver can lend it. A machine with [`CARD`] has that one for the stack
/// process, and a boot that wants no stack process lends nothing.
pub fn lends(unetcard: Option<&str>, unetstack: Option<&str>, card: bool, lendable: bool) -> bool {
    unetcard == Some("kernel")
        && lendable
        && !card
        && !matches!(unetstack, Some("kernel") | Some("none"))
}

/// Says what [`lends`] answered on this boot, once it is known.
pub fn set_lent(lent: bool) {
    LENT.store(lent, core::sync::atomic::Ordering::Release);
}

/// The card the kernel lends on this boot, by its name ([`LENT_CARD`]), if
/// it lends one.
pub fn lent() -> Option<&'static str> {
    LENT.load(core::sync::atomic::Ordering::Acquire)
        .then_some(LENT_CARD)
}

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
/// not (`card`), where the kernel lends its first card or not (`lent`:
/// [`lends`]), whose command line has these values of `unetstack=` and
/// `unetuse=` (`None` for a word that is not there), on a machine where
/// the kernel drives `kernel_cards` network cards itself, the loopback not
/// counted and the lent one counted.
pub fn whose(
    card: bool,
    lent: bool,
    unetstack: Option<&str>,
    unetuse: Option<&str>,
    kernel_cards: usize,
) -> Whose {
    match unetuse {
        Some("all") => Whose::AllSaid,
        Some("asked") => Whose::AskedSaid,
        _ if !lent && !has_stack(card, unetstack) => Whose::NoStack,
        _ => match kernel_cards.saturating_sub(lent as usize) {
            0 => Whose::AllOnlyCard,
            others => Whose::KernelCards(others),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stack_process_is_the_network_of_a_machine_whose_only_card_it_has() {
        // Nothing but the card left to userspace: the stack process takes
        // it, and every program has that stack.
        assert_eq!(whose(true, false, None, None, 0), Whose::AllOnlyCard);
        // With an address of its own or a server's, the same.
        assert_eq!(
            whose(true, false, Some("dhcp"), None, 0),
            Whose::AllOnlyCard
        );
        assert_eq!(
            whose(true, false, Some("10.0.9.15/24,10.0.9.2"), None, 0),
            Whose::AllOnlyCard
        );
        assert!(Whose::AllOnlyCard.all());
    }

    #[test]
    fn a_boot_that_leaves_no_card_to_a_stack_process_keeps_the_kernels() {
        // A machine without such a card, or a boot that did not leave it.
        assert_eq!(whose(false, false, None, None, 1), Whose::NoStack);
        assert_eq!(whose(false, false, None, None, 0), Whose::NoStack);
        // The card left to userspace for the kernel's own stack, or for
        // nobody's.
        assert_eq!(whose(true, false, Some("kernel"), None, 0), Whose::NoStack);
        assert_eq!(whose(true, false, Some("none"), None, 0), Whose::NoStack);
        // An address there does not make a stack of it.
        assert_eq!(whose(false, false, Some("dhcp"), None, 0), Whose::NoStack);
        assert!(!Whose::NoStack.all());
    }

    #[test]
    fn a_card_of_the_kernels_keeps_its_stack_for_who_does_not_ask() {
        assert_eq!(whose(true, false, None, None, 1), Whose::KernelCards(1));
        assert_eq!(
            whose(true, false, Some("dhcp"), None, 2),
            Whose::KernelCards(2)
        );
        assert!(!Whose::KernelCards(1).all());
    }

    #[test]
    fn what_the_boot_says_of_every_program_is_what_counts() {
        // `unetuse=all`: with a card of the kernel's, and even with no
        // stack process of the boot's (one can be run by hand).
        assert_eq!(whose(true, false, None, Some("all"), 1), Whose::AllSaid);
        assert_eq!(whose(false, false, None, Some("all"), 1), Whose::AllSaid);
        assert_eq!(
            whose(true, false, Some("kernel"), Some("all"), 0),
            Whose::AllSaid
        );
        assert!(Whose::AllSaid.all());
        // `unetuse=asked`: the stack has the only card and still only the
        // programs that ask have it.
        assert_eq!(whose(true, false, None, Some("asked"), 0), Whose::AskedSaid);
        assert_eq!(
            whose(true, false, Some("dhcp"), Some("asked"), 0),
            Whose::AskedSaid
        );
        assert!(!Whose::AskedSaid.all());
        // A word that is neither is as good as none.
        assert!(understood("all") && understood("asked"));
        assert!(!understood("") && !understood("ALL") && !understood("every"));
        assert_eq!(
            whose(true, false, None, Some("every"), 0),
            Whose::AllOnlyCard
        );
        assert_eq!(
            whose(true, false, None, Some("every"), 1),
            Whose::KernelCards(1)
        );
        assert_eq!(whose(false, false, None, Some(""), 0), Whose::NoStack);
    }

    #[test]
    fn a_card_the_kernel_lends_is_the_stack_processs_and_not_the_kernels() {
        // The boot asks, the kernel's first card can be lent and no virtio
        // card is left to userspace: lent.
        assert!(lends(Some("kernel"), None, false, true));
        assert!(lends(Some("kernel"), Some("dhcp"), false, true));
        // Not asked, or asked with a word that is not the one.
        assert!(!lends(None, None, false, true));
        assert!(!lends(Some("yes"), None, false, true));
        // No card of the kernel's, or one whose driver cannot lend it.
        assert!(!lends(Some("kernel"), None, false, false));
        // The virtio card is the stack process's first.
        assert!(!lends(Some("kernel"), None, true, true));
        // A boot with no stack process has nobody to lend to.
        assert!(!lends(Some("kernel"), Some("kernel"), false, true));
        assert!(!lends(Some("kernel"), Some("none"), false, true));

        // The only card of the machine, lent: the stack process is the
        // network of every program, as with a card of its own.
        assert_eq!(whose(false, true, None, None, 1), Whose::AllOnlyCard);
        // Another card the kernel keeps: its stack for who does not ask.
        assert_eq!(whose(false, true, None, None, 2), Whose::KernelCards(1));
        // What the boot says of every program still counts.
        assert_eq!(whose(false, true, None, Some("asked"), 1), Whose::AskedSaid);
        assert_eq!(whose(false, true, None, Some("all"), 2), Whose::AllSaid);

        // What `/proc/udrivers` lists it as, once the boot has decided.
        assert_eq!(lent(), None);
        set_lent(true);
        assert_eq!(lent(), Some(LENT_CARD));
        set_lent(false);
    }
}
