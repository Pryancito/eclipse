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
//! * `unetstack=none`: the card is not left to userspace unless the boot
//!   names it, no stack takes it either way, and the kernel lends none of
//!   its own: the machine has no network, and its programs the sockets
//!   of a machine with no card.
//!
//! No word gives a boot the kernel's own stack any more: `unetstack=kernel`,
//! `unetuse=asked` and `unetlend=asked` did, and say nothing now
//! ([`gone`]); nor does `unetuse=all`, which is every boot.
//!
//! On x86_64 a machine with no such card whose first card is an Intel
//! 82574 (QEMU's `e1000e`) leaves that one to userspace the same way
//! ([`E1000E_CARD`]), and the stack process is its driver;
//! `unetcard=kernel` keeps its driver in the kernel, which then lends it
//! as below.
//!
//! A machine with neither, whose first card the kernel drives itself
//! (an I219, or any card on a boot that says `unetcard=kernel`), lends that card to the stack process
//! ([`lends`]): the kernel goes on driving it, its frames go to the stack
//! process and not to the kernel's stack, and it counts as the stack
//! process's card in all of the above. That is the machine's network too
//! when the kernel has no other card. `unetstack=none` lends nothing;
//! `unetcard=kernel` lends the card even when a virtio card is there, which
//! is then not left to userspace unasked.
//!
//! Every driver of the kernel's can lend its card (`NetScheme::can_lend`),
//! and an ordinary boot lends whichever is first ([`lends_any`]). A small
//! echo costs about 1.6 times as much lent as with the kernel's own stack
//! (against a host that takes its time to answer; connecting, sending and
//! receiving are faster lent), which is the price that was accepted for
//! one stack instead of two. On
//! x86_64 a card with no MSI needs the legacy interrupt line of its slot
//! to be lent, which a boot has under a hypervisor or with `pci.intx=1`
//! (`kernel_hal::drivers::pci_intx_wanted`); without the line such a card
//! is lent only to a boot that says `unetlend=any`.
//!
//! The kernel's other cards are lent too ([`lends_cards`]), to the same
//! stack process, whichever its first card is: it has a stack for each
//! (`tools/eclipse-netstack`, `cards`), and is the machine's network by
//! whichever has a cable. A card that cannot be lent (it has no
//! interrupt to tell the borrower of its frames with: a card with no MSI
//! on a real x86_64 machine that does not say `pci.intx=1`) carries nothing: no program's
//! sockets are the kernel's own stack's, which was the only one that
//! reached it, and the boot says so ([`Whose::AllUnlent`]).
//!
//! A machine with no card at all, the kernel's or anybody's, has no stack
//! process and needs none: what its programs say to each other never
//! reaches a stack ([`Whose::AllNoCard`]). Their sockets are the served
//! ones all the same, which join two programs of the machine by
//! themselves, and the kernel's own stack carries nothing there.

/// The card the stack process takes, as `udriver=` names it and as
/// `zcore_drivers::user::reserved` lists it: the one the `unet` boot
/// service is started for.
pub const CARD: &str = "virtio-net@0";

/// The first Intel 82574 card, when the boot leaves it to userspace
/// (unasked on x86_64, or `udriver=e1000e@0`): the stack process claims it and is its driver,
/// as with [`CARD`], and it counts as that card in all of the above. The
/// `enet` boot service is started for it.
pub const E1000E_CARD: &str = "e1000e@0";

/// Whether what the buses left to userspace (`zcore_drivers::user::reserved`)
/// has a card for the stack process to take and drive: [`CARD`] or
/// [`E1000E_CARD`].
pub fn has_card<S: AsRef<str>>(left_to_userspace: &[S]) -> bool {
    left_to_userspace
        .iter()
        .any(|left| [CARD, E1000E_CARD].contains(&left.as_ref()))
}

/// The kernel's first card when it is lent to the stack process, as
/// `/proc/udrivers` lists it: the one the `knet` boot service is started
/// for.
pub const LENT_CARD: &str = "kernel-net@0";

static LENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How many of its cards the kernel lends on this boot, and whether the
/// ones whose drivers lend only when asked are among them.
static CARDS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static ANY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether this boot lends the kernel's first card to a stack that runs as
/// a process: `unetstack` is the value of that word, `card` is whether
/// [`CARD`] is there and left to userspace, and `lendable` whether the
/// kernel drives a first card and its driver can lend it. It does unless
/// one of those says no: a machine with [`CARD`] has that one for the stack
/// process, and a boot that wants no stack process (`unetstack=none`)
/// lends nothing.
pub fn lends(unetstack: Option<&str>, card: bool, lendable: bool) -> bool {
    lendable && !card && unetstack != Some("none")
}

/// What a word of the command line that kept the kernel's own stack for
/// a boot said, when `value` is one of those: the words are still read,
/// to tell whoever wrote them that they say nothing now.
pub fn gone(word: &str, value: &str) -> bool {
    matches!(
        (word, value),
        ("unetstack", "kernel") | ("unetuse", "asked" | "all") | ("unetlend", "asked")
    )
}

/// How many of the kernel's cards this boot lends to the stack that runs
/// as a process: `lendable` is how many it drives whose drivers can lend
/// them on this boot ([`lends_any`]), and `unetstack` the command line's
/// word. All of them, whether the stack process starts with one of them
/// or with a card of its own, unless the boot wants no stack process
/// (`unetstack=none`).
pub fn lends_cards(unetstack: Option<&str>, lendable: usize) -> usize {
    match unetstack {
        Some("none") => 0,
        _ => lendable,
    }
}

/// Says what [`lends_cards`] answered on this boot, once it is known, and
/// what [`lends_any`] did.
pub fn set_lends(cards: usize, any: bool) {
    CARDS.store(cards, core::sync::atomic::Ordering::Release);
    ANY.store(any, core::sync::atomic::Ordering::Release);
}

/// Whether this boot lends any card of the kernel's.
pub fn lends_some() -> bool {
    CARDS.load(core::sync::atomic::Ordering::Acquire) != 0
}

/// Whether a card of the kernel's may be borrowed now: `loans` is how
/// many are lent at the moment, and `unasked` whether the card's driver
/// lends it without the boot's word (`NetScheme::lends_unasked`).
pub fn may_borrow(loans: usize, unasked: bool) -> bool {
    loans < CARDS.load(core::sync::atomic::Ordering::Acquire)
        && (unasked || ANY.load(core::sync::atomic::Ordering::Acquire))
}

/// Whether this boot lends a card whose driver lends only when asked
/// (`NetScheme::lends_unasked`). `unetlend` is that word's value: `any`
/// says yes. A boot that does not say so lends it where
/// the card can be told of its frames: `interrupts` is whether a card with
/// no MSI has an interrupt on this boot (always, off x86_64; on x86_64,
/// whether it is given the legacy line of its slot).
pub fn lends_any(unetlend: Option<&str>, interrupts: bool) -> bool {
    match unetlend {
        Some("any") => true,
        _ => interrupts,
    }
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

/// What is behind the sockets of this boot's programs, which are the
/// served ones on every boot, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    /// The stack process, which has every card of the machine.
    AllOnlyCard,
    /// Nothing: the machine has no card, or the boot wants no stack, and
    /// what its programs say to each other needs none.
    AllNoCard,
    /// The stack process if it has a card (`stack`), and nothing
    /// otherwise; either way the kernel drives this many cards it cannot
    /// lend, which carry nothing.
    AllUnlent { cards: usize, stack: bool },
}

impl Whose {
    /// Whether no stack process is expected behind those sockets: what
    /// would leave the machine has no way out, and never will have.
    pub fn alone(self) -> bool {
        matches!(
            self,
            Whose::AllNoCard | Whose::AllUnlent { stack: false, .. }
        )
    }

    /// How many cards the kernel drives and cannot lend.
    pub fn unlent(self) -> usize {
        match self {
            Whose::AllUnlent { cards, .. } => cards,
            _ => 0,
        }
    }
}

/// Whether a stack that runs as a process has a card on this boot: `card`
/// is whether [`CARD`] is there and left to userspace, and `unetstack` the
/// value of that word on the command line.
pub fn has_stack(card: bool, unetstack: Option<&str>) -> bool {
    card && unetstack != Some("none")
}

/// What is behind the sockets on a boot where [`CARD`] is left to
/// userspace or not (`card`), where the kernel lends `lent` of its cards
/// ([`lends_cards`]), whose command line has this value of `unetstack=`
/// (`None` when the word is not there), on a machine where the kernel
/// drives `kernel_cards` network cards itself, the loopback not counted
/// and the lent ones counted.
pub fn whose(card: bool, lent: usize, unetstack: Option<&str>, kernel_cards: usize) -> Whose {
    let stack = lent != 0 || has_stack(card, unetstack);
    match kernel_cards.saturating_sub(lent) {
        // A boot that wants no stack (`unetstack=none`) lends nothing
        // because it says so, not because it cannot.
        _ if !stack && unetstack == Some("none") => Whose::AllNoCard,
        0 if stack => Whose::AllOnlyCard,
        0 => Whose::AllNoCard,
        cards => Whose::AllUnlent { cards, stack },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_card_the_kernel_can_lend_is_lent_unless_the_boot_wants_no_stack() {
        // With nothing said: all of them, to a stack process that starts
        // with one of them or with a card of its own.
        assert_eq!(lends_cards(None, 2), 2);
        assert_eq!(lends_cards(Some("dhcp"), 1), 1);
        assert_eq!(lends_cards(None, 0), 0);
        // No stack process: nobody to lend to.
        assert_eq!(lends_cards(Some("none"), 2), 0);
        // The word that kept the kernel's stack says nothing.
        assert_eq!(lends_cards(Some("kernel"), 2), 2);

        // What a borrower is answered by, once the boot has decided.
        assert!(!lends_some());
        assert!(!may_borrow(0, true));
        set_lends(2, false);
        assert!(lends_some());
        assert!(may_borrow(0, true) && may_borrow(1, true));
        assert!(!may_borrow(2, true), "as many as the boot lends");
        assert!(!may_borrow(0, false), "a driver that lends only when asked");
        set_lends(1, true);
        assert!(may_borrow(0, false));
        assert!(!may_borrow(1, false));
        set_lends(0, false);
    }

    #[test]
    fn an_ordinary_boot_lends_any_card_that_has_an_interrupt() {
        assert!(lends_any(None, true));
        assert!(!lends_any(None, false), "nothing to tell the borrower with");
        assert!(
            lends_any(Some("yes"), true),
            "a word it does not know is none"
        );
    }

    #[test]
    fn the_boot_can_say_which_cards_are_lent() {
        for interrupts in [false, true] {
            assert!(lends_any(Some("any"), interrupts));
            // The word that lent fewer says nothing now.
            assert_eq!(lends_any(Some("asked"), interrupts), interrupts);
        }
    }

    #[test]
    fn a_virtio_card_or_the_first_e1000e_left_to_userspace_is_the_stacks() {
        assert!(has_card(&["virtio-net@0"]));
        assert!(has_card(&["virtio-blk@1", "e1000e@0"]));
        assert!(!has_card(&["virtio-blk@1"]));
        assert!(!has_card(&["virtio-net@1", "e1000e@1"]));
        assert!(!has_card::<&str>(&[]));
    }

    #[test]
    fn the_stack_process_is_the_network_of_a_machine_whose_only_card_it_has() {
        // Nothing but the card left to userspace: the stack process takes
        // it, and every program has that stack.
        assert_eq!(whose(true, 0, None, 0), Whose::AllOnlyCard);
        // With an address of its own or a server's, the same.
        assert_eq!(whose(true, 0, Some("dhcp"), 0), Whose::AllOnlyCard);
        assert_eq!(
            whose(true, 0, Some("10.0.9.15/24,10.0.9.2"), 0),
            Whose::AllOnlyCard
        );
        assert!(!Whose::AllOnlyCard.alone());
        assert_eq!(Whose::AllOnlyCard.unlent(), 0);
    }

    #[test]
    fn a_card_the_kernel_cannot_lend_carries_nothing() {
        // A machine whose cards the kernel drives and cannot lend: the
        // served sockets, with nothing behind them.
        let alone = |cards| Whose::AllUnlent {
            cards,
            stack: false,
        };
        assert_eq!(whose(false, 0, None, 1), alone(1));
        assert_eq!(whose(false, 0, Some("dhcp"), 2), alone(2));
        assert!(alone(1).alone());
        assert_eq!(alone(2).unlent(), 2);
        // Beside a card the stack process has: that one is the network.
        let beside = |cards| Whose::AllUnlent { cards, stack: true };
        assert_eq!(whose(true, 0, None, 1), beside(1));
        assert_eq!(whose(true, 0, Some("dhcp"), 2), beside(2));
        assert_eq!(whose(false, 1, None, 2), beside(1));
        assert_eq!(whose(true, 1, None, 2), beside(1));
        assert!(!beside(1).alone());
        assert_eq!(beside(1).unlent(), 1);
    }

    #[test]
    fn a_machine_with_no_card_has_the_served_sockets_and_no_stack() {
        // No card of the kernel's, none left to userspace.
        assert_eq!(whose(false, 0, None, 0), Whose::AllNoCard);
        // An address there does not make a stack of it.
        assert_eq!(whose(false, 0, Some("dhcp"), 0), Whose::AllNoCard);
        // A boot that wants no stack is nobody's network, whatever
        // cards it has, and none of them is one it could not lend.
        assert_eq!(whose(true, 0, Some("none"), 0), Whose::AllNoCard);
        assert_eq!(whose(false, 0, Some("none"), 0), Whose::AllNoCard);
        assert_eq!(whose(true, 0, Some("none"), 1), Whose::AllNoCard);
        assert_eq!(whose(false, 0, Some("none"), 2), Whose::AllNoCard);
        assert!(Whose::AllNoCard.alone());
        assert_eq!(Whose::AllNoCard.unlent(), 0);
    }

    #[test]
    fn no_word_gives_a_boot_the_kernels_stack() {
        assert!(gone("unetstack", "kernel"));
        assert!(gone("unetuse", "asked") && gone("unetuse", "all"));
        assert!(gone("unetlend", "asked"));
        assert!(!gone("unetstack", "none") && !gone("unetstack", "dhcp"));
        assert!(!gone("unetuse", "every") && !gone("unetlend", "any"));
        assert!(!gone("unetcard", "kernel"));
        // With them said, the boot is the one that says nothing: the
        // stack process has the card and every program its sockets.
        assert_eq!(whose(true, 0, Some("kernel"), 0), Whose::AllOnlyCard);
        assert_eq!(whose(false, 0, Some("kernel"), 0), Whose::AllNoCard);
        assert!(lends(Some("kernel"), false, true));
        assert!(has_stack(true, Some("kernel")));
    }

    #[test]
    fn a_card_the_kernel_lends_is_the_stack_processs_and_not_the_kernels() {
        // The kernel's first card can be lent and no virtio card is left
        // to userspace: lent, with nothing said or with an address.
        assert!(lends(None, false, true));
        assert!(lends(Some("dhcp"), false, true));
        assert!(lends(Some("10.0.2.15/24,10.0.2.2"), false, true));
        // No card of the kernel's, or one whose driver cannot lend it.
        assert!(!lends(None, false, false));
        // The virtio card is the stack process's first.
        assert!(!lends(None, true, true));
        // A boot with no stack process has nobody to lend to.
        assert!(!lends(Some("none"), false, true));

        // The only card of the machine, lent: the stack process is the
        // network of every program, as with a card of its own.
        assert_eq!(whose(false, 1, None, 1), Whose::AllOnlyCard);
        // Every card of the kernel's lent: the stack process has them
        // all, with a card of its own or without.
        assert_eq!(whose(false, 2, None, 2), Whose::AllOnlyCard);
        assert_eq!(whose(true, 1, None, 1), Whose::AllOnlyCard);

        // What `/proc/udrivers` lists it as, once the boot has decided.
        assert_eq!(lent(), None);
        set_lent(true);
        assert_eq!(lent(), Some(LENT_CARD));
        set_lent(false);
    }
}
